// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Safepoint polls, shadow-stack publication and oop maps.
//!
//! The collector half of the compiled frame. A safepoint is only sound if
//! every live reference is findable when the collector stops the world, so
//! each poll site has to spill the operand stack, publish register-homed oops,
//! and record a map the collector can walk and — under a moving young
//! generation — rewrite.
//!
//! `can_elide_self_call_register_spill` and
//! `moving_young_safepoint_coverage_complete` are the two places where that
//! obligation is discharged by proof rather than by emission; both are
//! deliberately conservative, because the failure mode of being wrong here is
//! a reclaimed live object rather than a wrong answer.

use super::*;

/// The three bytes before the imm32 of `CMP r64, imm32` — `REX.W [+ REX.B]`,
/// opcode `0x81`, ModRM `mod=11 /7 rm=r`.
///
/// A free function with a test rather than three inline literals, because the
/// inline version carried REX.B unconditionally (`0x49`) on the strength of a
/// comment asserting the register was always `r8..r15`. It is not: shadow homes
/// come from `SCRATCH_REGS` **or** `LOCAL_REGS`, and `LOCAL_REGS` holds RBX on
/// every platform and RSI/RDI on Windows. Forcing REX.B on rewrites the ModRM
/// `r/m` field to `r + 8`, i.e. compares an entirely different register.
/// The synthetic bytecode pc the METHOD-ENTRY safepoint poll records.
///
/// No real method's bytecode is anywhere near 4 GiB, so this can never collide
/// with a genuine bci in the sp-id-keyed oop-map lookup — which is the whole
/// reason `emit_safepoint_poll_prologue` uses it instead of the `0` that
/// `cur_bc_pc` still holds at that point. Named rather than spelled
/// `u32::MAX as usize` at each site, because `Compiler::local_oop_mask_at_current_pc`
/// has to recognise it and a bare literal is not a thing a reader can look up.
pub(super) const ENTRY_POLL_BC_PC: usize = u32::MAX as usize;

/// "This frame has not reached a safepoint yet" -- the value the single-pass
/// prologue stamps into the reserved safepoint-id slot.
///
/// The IR backend can use `0` for this, because its ids are a monotonic
/// counter that starts at 1. This backend stores the BYTECODE PC, and **bci 0
/// is legal** -- a constructor or a method that opens with `new`/an `invoke` is
/// a very common shape -- so `0` there is ambiguous between "at bci 0" and
/// "never stored", and stamping it would let an unsafepointed frame match the
/// bci-0 map and be relocated against it.
///
/// `u32::MAX - 1` is unambiguous from both sides: no real bytecode pc is
/// anywhere near 4 GiB, and it is distinct from [`ENTRY_POLL_BC_PC`], the other
/// synthetic pc this backend uses. `find_oop_map_for_safepoint_id` finds
/// nothing for it, so the coverage proof fails CLOSED -- which is the point.
/// The hazard it removes is the one that is NOT loud: an uninitialised slot can
/// read as a valid id for the method standing at that rbp, and relocation then
/// rewrites against the wrong program point's map.
pub(super) const SP_ID_UNSET_BC_PC: usize = (u32::MAX - 1) as usize;

pub(super) const fn cmp_r64_imm32_opcode(r: u8) -> [u8; 3] {
    // 0x48 = REX.W; |0x01 adds REX.B, needed only for r8..r15.
    let rex = 0x48 | if r >= 8 { 0x01 } else { 0x00 };
    [rex, 0x81, 0xC0 | (7 << 3) | (r & 7)]
}

/// Why a safepoint's oop map was recorded as INCOMPLETE, counted per cause.
///
/// `emit_oop_map_for_safepoint` withholds such a safepoint's pc from
/// `mapped_safepoint_pcs`, which turns `CompiledMethod::fully_oop_covered`
/// false for the whole method, which the runtime reports as one
/// `map_coverage=N` counter on the `[jitroots]` line. That aggregate is where
/// `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821.md` ran out of
/// road: it names the field, never the reason, and the six ways to get there
/// want six different repairs.
///
/// Process-global relaxed counters — compilation is not on any hot path and
/// these are read once, at the end of a run, by `CRATONVM_DBG_OOPCOV`.
pub mod map_incomplete_cause {
    use std::sync::atomic::AtomicUsize;
    /// The operand-stack oop mark vector was not exact at this safepoint.
    pub static MARKS_INEXACT: AtomicUsize = AtomicUsize::new(0);
    /// A marked operand-stack oop was still register/scratch/xmm resident.
    pub static OOP_STILL_IN_REGISTER: AtomicUsize = AtomicUsize::new(0);
    /// An operand-stack frame slot sits further than `i16::MAX` from `rbp`.
    pub static STACK_OFF_TOO_DEEP: AtomicUsize = AtomicUsize::new(0);
    /// A local-variable home sits further than `i16::MAX` from `rbp`.
    pub static LOCAL_OFF_TOO_DEEP: AtomicUsize = AtomicUsize::new(0);
    /// A staged invoke-argument home sits further than `i16::MAX` from `rbp`.
    pub static STAGED_ARG_OFF_TOO_DEEP: AtomicUsize = AtomicUsize::new(0);
    /// A reference was staged somewhere no map can name (native-ABI outgoing
    /// args, direct-call service slots, inlined-callee parameter locals).
    pub static STAGED_ARG_UNMAPPABLE: AtomicUsize = AtomicUsize::new(0);
    /// A live inline (spliced-callee) scope could not be named: its local home
    /// sits further than `i16::MAX` from `rbp`, or its dataflow could not
    /// classify the callee pc at all. See `Compiler::inline_oop_scopes`.
    pub static INLINE_LOCAL_UNMAPPABLE: AtomicUsize = AtomicUsize::new(0);

    /// The local-oop dataflow never reached this safepoint's pc, so the map
    /// names NO reference locals for it.
    ///
    /// `record_oop_map` reads the locals through `local_oop_mask_at_current_pc()`
    /// inside an `if let Some(..)` with no `else`: a `None` there contributes no
    /// slots, sets no `map_incomplete`, and bumps nothing, so the safepoint
    /// would publish a map claiming complete coverage while every live reference
    /// local is unnamed. That is a real hole in the shape of the one
    /// `bug-h2-testrandommapops-small-heap-corruption-20260829.md` is about.
    ///
    /// **Measured 2026-09-02, and it reads ZERO** on every method of
    /// `probes/SafepointMapResidue.java`, including the one whose frames the
    /// residue instrument flags. The dataflow does reach those safepoints and
    /// does hand back a mask, and the mask is correct — `javap -c` on the
    /// flagged method shows the slot the instrument called a missed root is an
    /// `int` local at that pc. Kept as a ruled-out hypothesis rather than a
    /// live lead: the `tlab_retire_skipped` pattern, where the point of
    /// printing a zero is that the hypothesis it eliminates is a good one.
    ///
    /// The hole it guards is still real even though it has not been observed:
    /// a `None` from `local_oop_mask_at_current_pc()` would contribute no
    /// slots, set no `map_incomplete` and bump nothing, so the safepoint would
    /// publish a map claiming complete coverage while every live reference
    /// local went unnamed. This counter is what would show that happening.
    pub static LOCAL_MASK_UNREACHED: AtomicUsize = AtomicUsize::new(0);

    /// How many causes [`snapshot`] returns.
    ///
    /// Named so the census printer can assert against it. `driver.rs` printed
    /// SIX of these seven for as long as the seventh existed, which made a run
    /// whose only unnameable references were inline-scope locals read as
    /// `causes(... all zero)` -- "no cause", from a cause census. The
    /// 2026-08-30 diagnosis that concluded "One cause, `staged_unmappable`"
    /// was made from that line.
    pub const COUNT: usize = 8;

    /// `(marks_inexact, oop_in_register, stack_deep, local_deep, staged_deep,
    /// staged_unmappable, inline_local_unmappable, local_mask_unreached)`.
    pub fn snapshot() -> [usize; COUNT] {
        use std::sync::atomic::Ordering::Relaxed;
        [
            MARKS_INEXACT.load(Relaxed),
            OOP_STILL_IN_REGISTER.load(Relaxed),
            STACK_OFF_TOO_DEEP.load(Relaxed),
            LOCAL_OFF_TOO_DEEP.load(Relaxed),
            STAGED_ARG_OFF_TOO_DEEP.load(Relaxed),
            STAGED_ARG_UNMAPPABLE.load(Relaxed),
            INLINE_LOCAL_UNMAPPABLE.load(Relaxed),
            LOCAL_MASK_UNREACHED.load(Relaxed),
        ]
    }
}

/// Why a safepoint's SHADOW claim (`OopMapEntry::moving_young_coverage_complete`)
/// came out false, counted per cause.
///
/// The sibling of [`map_incomplete_cause`], for the other of the two coverage
/// notions. `CompiledMethod::fully_shadow_covered` ANDs this flag over every
/// safepoint of a method, and the OSR fallback reads that aggregate — so a
/// single false here refuses relocation for every collection with one of this
/// method's frames live. Six causes, six different repairs.
pub mod shadow_incomplete_cause {
    use std::sync::atomic::AtomicUsize;
    /// Moving-young is off, or the compile already failed.
    pub static GATE_OFF_OR_FAILED: AtomicUsize = AtomicUsize::new(0);
    /// `stack.len() != stack_oop_marks.len()` — the lockstep invariant broke.
    pub static MARK_VECTOR_DESYNC: AtomicUsize = AtomicUsize::new(0);
    /// The operand-stack oop marks are not exact at this safepoint (a revived
    /// dead-code merge reconstructed the stack at a nonzero depth).
    pub static MARKS_INEXACT: AtomicUsize = AtomicUsize::new(0);
    /// A marked oop is in a scratch or XMM slot, which the push cannot name.
    pub static OOP_IN_SCRATCH_OR_XMM: AtomicUsize = AtomicUsize::new(0);
    /// More than 64 locals, so `color_graph`'s mask cannot describe them all.
    pub static TOO_MANY_LOCALS: AtomicUsize = AtomicUsize::new(0);
    /// The forward "must be oop" dataflow never reached this bytecode pc, so
    /// there is no local oop mask to publish from.
    pub static LOCAL_OOP_DATAFLOW_UNREACHED: AtomicUsize = AtomicUsize::new(0);
    /// The push was not emitted at all (shadow gate off, helper unwired, or the
    /// prologue reserved no thread slot).
    pub static PUSH_NOT_EMITTED: AtomicUsize = AtomicUsize::new(0);
    /// A live inline (spliced-callee) scope could not say which of its locals
    /// hold references at the callee pc being emitted, so the splice's frame
    /// slots cannot be published. See `Compiler::inline_oop_scopes`.
    pub static INLINE_SCOPE_UNMAPPED: AtomicUsize = AtomicUsize::new(0);

    /// `(gate_off, desync, marks_inexact, oop_in_scratch, too_many_locals,
    /// dataflow_unreached, push_not_emitted, inline_scope_unmapped)`.
    pub fn snapshot() -> [usize; 8] {
        use std::sync::atomic::Ordering::Relaxed;
        [
            GATE_OFF_OR_FAILED.load(Relaxed),
            MARK_VECTOR_DESYNC.load(Relaxed),
            MARKS_INEXACT.load(Relaxed),
            OOP_IN_SCRATCH_OR_XMM.load(Relaxed),
            TOO_MANY_LOCALS.load(Relaxed),
            LOCAL_OOP_DATAFLOW_UNREACHED.load(Relaxed),
            PUSH_NOT_EMITTED.load(Relaxed),
            INLINE_SCOPE_UNMAPPED.load(Relaxed),
        ]
    }
}

impl Compiler {
    // -----------------------------------------------------------------------
    // Frame layout, prologue and epilogue
    // -----------------------------------------------------------------------
    //
    // Moved to `x64/frames.rs`. The offsets computed there are read by the oop
    // maps, the deopt snapshots and the OSR trampoline, so they are a contract
    // rather than a private detail.

    pub(super) fn emit_pre_safepoint_spill(&mut self) {
        self.emit_pre_safepoint_spill_impl(true);
    }

    /// [`Self::emit_pre_safepoint_spill`] for a site that has already written
    /// every Java argument of its call into a frame slot, so the blind spill
    /// may drop `RAX` and `ARG_REGS` from its selection.
    ///
    /// The one-shot is set and consumed in the same breath — this function is
    /// the only writer, and the spill it calls takes the flag at its top — so a
    /// site that stages and then does NOT reach the spill (the elision wins)
    /// cannot leak the claim into the next safepoint. See
    /// [`spill_args_published_enabled`] for what the four opting-in sites have
    /// actually done by this point.
    pub(super) fn emit_pre_safepoint_spill_args_published(
        &mut self,
        arg_regs_published: bool,
        rax_published: bool,
    ) {
        let on = spill_args_published_enabled();
        self.args_published_for_next_spill = (arg_regs_published && on, rax_published && on);
        self.emit_pre_safepoint_spill_impl(true);
    }

    /// Publish a cold safepoint without claiming moving-young shadow coverage.
    /// This is used only by the overflow guard of a GC-inert recursive method;
    /// an exceptional collection safely falls back to the non-moving sweep.
    pub(super) fn emit_pre_safepoint_spill_without_shadow(&mut self) {
        self.emit_pre_safepoint_spill_impl(false);
    }

    fn emit_pre_safepoint_spill_impl(&mut self, publish_shadow: bool) {
        // Handshake with `emit_inline_tlab_new` (see `alloc_spill_sink_enabled`).
        // The `new` site raises `sink_alloc_blind_spill` immediately before
        // calling us; we withhold every register the allocation fast path does
        // NOT clobber and acknowledge on `deferred_alloc_blind_spill`, which the
        // inline emitter consumes at its slow-path label.
        //
        // Taken ABOVE the `failed` guard so an abandoned compile cannot leave the
        // request standing for a LATER safepoint, which would withhold eleven
        // registers with nobody to emit them. Cleared unconditionally so a
        // request this safepoint cannot honour (spill disabled, `nostore`, or the
        // callee-saved-only `=1` mode, whose slot layout is `alloc_used_regs`-
        // indexed rather than `ALL_SPILL_GPRS`-indexed) leaves the consumer with
        // nothing to emit and the full spill in place.
        let sink = std::mem::take(&mut self.sink_alloc_blind_spill);
        // Taken here, next to `sink`, and for the same reason: a claim that
        // outlives the safepoint it was made for is a claim about the wrong
        // frame. Cleared even on the `failed` return below.
        let (arg_regs_published, rax_published) =
            std::mem::take(&mut self.args_published_for_next_spill);
        self.deferred_alloc_blind_spill = false;
        if self.failed {
            return;
        }
        // Bisect lever, default = current behaviour. `CRATONVM_JIT_MY_SCRATCH_FLUSH=0`
        // drops this flush when relocation is vetoed anyway.
        //
        // Why it is a candidate: this runs at EVERY GC-capable safepoint under
        // `moving_young`, and it is one of the few remaining costs that
        // `CRATONVM_NO_MOVING_YOUNG=1` removes but the relocation-scoped
        // admission gates do not. The `type.temporal` Hibernate classes still
        // exceed the 300 s cap on default flags while passing under that
        // variable, so a residual of this shape is unaccounted for. Shadow
        // push/reload has already been eliminated as the cause (measured
        // no-change; see `shadow_stack_maps_enabled`), which leaves this and
        // the self-call spill-elision proof.
        //
        // The lever question is answered: measured on `BinTreesClassic 18`
        // @512m, five interleaved reps, `CRATONVM_JIT_MY_SCRATCH_FLUSH=0`
        // moves nothing (median 4117 ms against a 4281 ms default, ranges
        // overlapping in both directions), so this is NOT the `type.temporal`
        // residual it was added to bisect.
        //
        // TRIED AND REVERTED 2026-07-31 — and the RECORDED REASON WAS WRONG.
        // Corrected 2026-08-03. Dropping the `moving_young_enabled()` term (so
        // the flush also runs in the non-moving lane) was reverted because that
        // lane then SIGILL'd 2/2, and the revert note blamed THIS call site:
        // "calling it here reserves spill slots and rewrites `self.stack` at a
        // point the non-precise frame layout did not budget for". It does not.
        // The SIGILL was the inline-PIC cascade's inter-slot `JNE` truncating
        // to `rel8` and branching backwards into the blind spill run emitted a
        // few lines below (fixed in `7f1b1f263`); ANY change that pushed a PIC
        // slot body past 127 bytes reproduced it, and this one did. Re-tested
        // 2026-08-03 with the truncation fixed — term removed,
        // `CRATONVM_GC=-moving-young`, Hibernate `ZonedDateTimeTest`, run
        // INTERLEAVED with a pre-fix build as a positive control: control
        // SIGILL 2/2 (1st and 5th), this variant clean 3/3.
        //
        // The term nevertheless STAYS, now for a reason about this mechanism
        // rather than about a crash: in the non-moving lane the full-GPR blind
        // spill below (`safepoint_reg_spill_all`, default-on since the same
        // day) already copies every caller-saved register into a frame slot the
        // conservative scan reads, so the flush buys no root visibility there —
        // only per-safepoint code size. Under moving-young it is NOT redundant:
        // it also rewrites `self.stack`, so the PRECISE map names those slots,
        // and a moving cycle has no conservative backstop to fall back on.
        // See `jit-no-moving-young-opt-out-unpublishes-roots-CLOSED-20260803.md`.
        if moving_young_enabled() && scratch_flush_at_safepoint_enabled() {
            self.flush_scratch_registers();
        }
        // Capture the live-frame bound for the map this safepoint will record.
        // Taken here rather than in `emit_oop_map_for_safepoint` because that
        // runs AFTER the call, by which point `emit_stack_arg_cleanup` may have
        // moved the cursor. Includes the staged invoke-argument buffer, which
        // sits above the operand stack in the same spill reserve and is live
        // for the duration of the call.
        self.pending_live_frame_hi = self.next_spill_offset;
        for idx in 0..self.local_assignments.len() {
            if let Some(reg) = self.local_assignments[idx] {
                let off = self.local_offset(idx);
                self.emit_store_local(off, reg);
            }
        }
        // SB-CRASH-04 (register-invisibility) — blind-spill the CURRENT value of
        // every used callee-saved GPR into its reserved frame slot. The local
        // flush above only covers register-resident *locals*; an oop can also
        // live in a callee-saved register as an operand-stack temporary that
        // survives the call, or via a value the per-slot oop tracker fails to
        // tag. Spilling ALL of `alloc_used_regs` is fully conservative: the
        // scanner re-validates each slot via `heap.is_object_address`, so non-
        // oop register values are simply ignored. Under the default non-moving
        // young sweep no post-call reload is needed (the object never moves, so
        // the register keeps a valid address). The slots are scanned by the
        // conservative `[scanner_sp, entry_sp)` frame walk. `=nostore` reserves
        // the slots but skips the stores (frame-perturbation A/B control).
        if self.safepoint_reg_spill && !self.safepoint_reg_spill_nostore && self.reg_spill_base != 0
        {
            // Gap 9 (`=all`): spill the FULL GPR file so a live oop in a
            // caller-saved / argument / RAX register (e.g. an invoke receiver
            // staged in an ARG reg, which the callee-saved-only spill misses) is
            // visible to the conservative root scan. `emit_store_local(off, reg)`
            // only reads `reg` (a plain `mov [rbp-off], reg`), so spilling the
            // arg registers here does not perturb the pending call's arguments.
            // Default path is unchanged (`=1` → callee-saved only).
            if self.safepoint_reg_spill_all {
                let narrow = self.oop_capable_spill_regs(arg_regs_published, rax_published);
                self.pending_narrow_spill = narrow;
                // Cast: `count_ones` is at most 14, well inside u64.
                let kept =
                    narrow.map_or(ALL_SPILL_GPRS.len() as u64, |m| u64::from(m.count_ones()));
                crate::metrics::note_spill_width(
                    kept,
                    ALL_SPILL_GPRS.len() as u64,
                    narrow.is_none(),
                );
                if sink {
                    self.emit_blind_reg_spill(|reg| {
                        ALLOC_FAST_PATH_CLOBBERS.contains(&reg) && Self::spill_selects(narrow, reg)
                    });
                    self.deferred_alloc_blind_spill = true;
                } else {
                    self.emit_blind_reg_spill(|reg| Self::spill_selects(narrow, reg));
                }
            } else {
                for i in 0..self.alloc_used_regs.len() {
                    let reg = self.alloc_used_regs[i];
                    let off = self.reg_spill_base + (i as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_store_local(off, reg);
                }
            }
        }
        // Stage 3 — record WHICH safepoint is active by storing the current
        // bytecode PC into the reserved safepoint-id slot. The GC root walker
        // reads `[rbp - sp_id_slot_off]` to recover the exact oop map (matched
        // on `OopMapEntry::bytecode_pc`). RAX is caller-saved and not an
        // argument register, and args are already staged in ARG_REGS before
        // this call, so clobbering RAX here is safe. Gated off by default.
        if self.precise_maps && self.sp_id_slot_off != 0 {
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_SPID").is_some() {
                eprintln!(
                    "[DBG_SPID] cur_bc_pc={} sp_id_slot_off={}",
                    self.cur_bc_pc, self.sp_id_slot_off
                );
            }
            self.emit_mov_imm32_sx(RAX, self.cur_bc_pc as i32); // Cast: bytecode PC fits i32
            self.emit_store_local(self.sp_id_slot_off, RAX);
            // Stage A.2 — this is a GC-capable safepoint that flushed its
            // register-locals and stored an sp-id; record it so finalize can
            // require a matching oop map (coverage = spilled ⊆ mapped).
            // Cast: bytecode/native offset to u32 (non-negative, fits)
            self.safepoint_pcs.insert(self.cur_bc_pc as u32);
        }
        // Shadow-stack precise roots — push every live oop onto the thread's
        // shadow stack so a moving collector can rewrite it precisely. Paired
        // with `emit_shadow_reload` in `emit_oop_map_for_safepoint`. Gated.
        if publish_shadow {
            self.emit_shadow_push();
        } else {
            self.pending_shadow.clear();
            self.pending_shadow_coverage_complete = false;
        }
    }

    /// The registers that can hold an oop at THIS safepoint, as a bitmask over
    /// [`ALL_SPILL_GPRS`] positions. `None` means "cannot prove anything here"
    /// and keeps the full fourteen-store copy.
    ///
    /// Four sources, and the first two are the reason `=all` exists at all
    /// (`Compiler::new`: "a receiver/args staged into ARG_REGS immediately
    /// before a GC-capable call ... which the callee-saved-only spill never
    /// covers"):
    ///
    ///  1. `RAX` — where the emitter materialises every loaded/allocated/
    ///     returned reference before it is pushed or stored;
    ///  2. every `ARG_REGS` member — a staged receiver or reference argument;
    ///  3. the register home of any local the method-wide reference mask says
    ///     can hold a reference (`register_homed_reference_locals` indexed
    ///     through `local_assignments`);
    ///  4. any register currently holding an operand-stack entry, oop-marked or
    ///     not — cheaper to keep than to reason about, and `self.stack` is
    ///     short.
    ///
    /// What is dropped is a register hosting a local the mask says is
    /// primitive, and a register this method's model never put anything in
    /// (R10/R11 on SysV, plus unused local homes). See
    /// [`narrow_safepoint_spill_enabled`] for what that gives up and why the
    /// stale slot it leaves behind is safe.
    fn oop_capable_spill_regs(&self, arg_regs_published: bool, rax_published: bool) -> Option<u16> {
        if !narrow_safepoint_spill_enabled() {
            return None;
        }
        // An explicit `CRATONVM_JIT_SAFEPOINT_REG_SPILL=all` is a request for
        // the full blind copy; honour it literally.
        if safepoint_reg_spill_all() {
            return None;
        }
        let plan = self.safepoint_publish.as_ref()?;
        // The mask cannot represent locals >= 64. `color_graph` never gives one
        // a register home, so no register is actually at risk -- but this is a
        // root-visibility decision, so it fails closed rather than reasoning.
        if self.num_locals > 64 {
            return None;
        }
        let bit = |reg: u8| -> u16 {
            match ALL_SPILL_GPRS.iter().position(|&r| r == reg) {
                // Cast: position < 14 < 16, so the shift is in range.
                Some(i) => 1u16 << i,
                None => 0,
            }
        };
        // Sources 1 and 2 -- RAX and the ABI argument registers -- each kept
        // unless the site has already written its contents to the frame, in
        // which case the register copy duplicates a publication that already
        // happened. The two are asked separately because different code
        // publishes them: see `args_published_for_next_spill`.
        let mut keep = 0u16;
        if !rax_published {
            keep |= bit(RAX);
        }
        if !arg_regs_published {
            for &r in ARG_REGS.iter() {
                keep |= bit(r);
            }
        }
        if arg_regs_published || rax_published {
            crate::metrics::note_spill_args_published();
        }
        let refs = plan.register_homed_reference_locals;
        for (idx, home) in self.local_assignments.iter().enumerate().take(64) {
            if refs & (1u64 << idx) != 0 {
                if let Some(reg) = *home {
                    keep |= bit(reg);
                }
            }
        }
        for slot in self.stack.iter() {
            match *slot {
                StackSlot::CalleeSaved(r) | StackSlot::Scratch(r) => keep |= bit(r),
                StackSlot::Frame(_) | StackSlot::Xmm(_) => {}
            }
        }
        Some(keep)
    }

    /// `true` when `reg` is selected by `mask` (`None` selects everything).
    fn spill_selects(mask: Option<u16>, reg: u8) -> bool {
        match mask {
            None => true,
            Some(m) => match ALL_SPILL_GPRS.iter().position(|&r| r == reg) {
                Some(i) => m & (1u16 << i) != 0,
                None => false,
            },
        }
    }

    /// Emit the `=all` blind GPR spill for the registers `want` selects, into
    /// their fixed `reg_spill_base + i*8` slots. The slot layout is indexed by
    /// position in [`ALL_SPILL_GPRS`] and does not depend on the selection, so a
    /// spill split across two program points (the sink) writes exactly the slots
    /// a single full spill would have.
    fn emit_blind_reg_spill(&mut self, want: impl Fn(u8) -> bool) {
        for (i, &reg) in ALL_SPILL_GPRS.iter().enumerate() {
            if !want(reg) {
                continue;
            }
            let off = self.reg_spill_base + (i as i32) * 8; // Cast: x86-64 immediate encoding
            self.emit_store_local(off, reg);
        }
    }

    /// Emit the half of the blind GPR spill that
    /// [`Self::emit_pre_safepoint_spill`] withheld at an inline-TLAB `new`, at
    /// the allocation's slow-path label. Returns whether anything was emitted.
    ///
    /// Every register written here is one the fast path provably does not touch
    /// (see [`ALLOC_FAST_PATH_CLOBBERS`]), so its value at the slow-path label
    /// is still its value at the safepoint. The three it does touch were already
    /// spilled at the safepoint itself.
    pub(super) fn emit_deferred_alloc_blind_spill(&mut self) -> bool {
        if !std::mem::take(&mut self.deferred_alloc_blind_spill) || self.failed {
            return false;
        }
        // The SAME selection the safepoint made -- see `pending_narrow_spill`.
        let narrow = self.pending_narrow_spill;
        self.emit_blind_reg_spill(|reg| {
            !ALLOC_FAST_PATH_CLOBBERS.contains(&reg) && Self::spill_selects(narrow, reg)
        });
        true
    }

    /// Publish the precise-map safepoint id without conservatively copying the
    /// whole GPR file into the frame. This is used only when
    /// [`Self::can_elide_self_call_register_spill`] proves that no live oop at
    /// the direct recursive call resides exclusively in a register. Under
    /// moving-young the proof is stronger: coverage must be complete and the
    /// exact live-oop home set must be empty, so the empty precise map itself is
    /// the complete root publication and no shadow slots need push/reload.
    pub(super) fn emit_safepoint_metadata_only(&mut self) {
        if self.failed {
            return;
        }
        // Must mirror `can_elide_self_call_register_spill` exactly — see the
        // note there. Both read `self_call_moving_proof_enabled()`.
        if self.shadow_enabled || self_call_moving_proof_enabled() {
            let coverage_complete = self.moving_young_safepoint_coverage_complete();
            let live_oop_home_count = self.collect_live_oop_homes().len();
            if !moving_oop_free_self_call_is_publishable(
                self_call_moving_proof_enabled(),
                coverage_complete,
                live_oop_home_count,
            ) {
                // This should be unreachable because the caller uses the same
                // predicate. Fail compilation closed if future call-site
                // refactoring breaks that pairing.
                self.fail("singlepass-codegen/self-call-moving-proof-unpublishable");
                return;
            }
            // Match the metadata state normally established by
            // `emit_pre_safepoint_spill` + an empty `emit_shadow_push`.
            self.pending_live_frame_hi = self.next_spill_offset;
            self.pending_shadow.clear();
            self.pending_shadow_coverage_complete = true;
        }
        if self.precise_maps && self.sp_id_slot_off != 0 {
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_SPID").is_some() {
                eprintln!(
                    "[DBG_SPID] cur_bc_pc={} sp_id_slot_off={} (metadata-only)",
                    self.cur_bc_pc, self.sp_id_slot_off
                );
            }
            self.emit_mov_imm32_sx(RAX, self.cur_bc_pc as i32);
            self.emit_store_local(self.sp_id_slot_off, RAX);
            self.safepoint_pcs.insert(self.cur_bc_pc as u32);
        }
    }

    /// Cooperative JIT safepoint poll (`CRATONVM_JIT_SAFEPOINT_POLLS`, see
    /// [`jit_safepoint_polls_enabled`]) — emits:
    /// ```asm
    /// MOV  R11, imm64            ; helpers.safepoint_flag_addr
    /// TEST byte ptr [R11], 0xFF  ; nonzero => STW requested
    /// JZ   .no_poll
    ///   <emit_pre_safepoint_spill>              ; frame-slot oop map valid
    ///   CALL helpers.safepoint_slow_path
    ///   <emit_oop_map_for_safepoint>             ; precise/shadow modes only
    /// .no_poll:
    /// ```
    /// matching the `self_call_stack_guard` call sequence's spill/call/oop-map
    /// bracketing exactly (see that call site in the direct self-recursive
    /// call arm). x86-64 has no `CMP [m64], imm` form that takes a bare
    /// absolute address, so the flag address is first materialized into the
    /// scratch register R11 (never a Java-local home — see `LOCAL_REGS` —
    /// nor an `ARG_REGS`/`SCRATCH_REGS` member, so it is always free to
    /// clobber here) via `MOV R11, imm64`, then read with a single non-atomic
    /// byte `TEST`.
    ///
    /// The slow helper resolves the current VM and Java thread from published
    /// process state/TLS, so the sequence is valid in pure methods too.
    pub(super) fn emit_safepoint_poll(&mut self) {
        if self.failed {
            return;
        }
        if !jit_safepoint_polls_enabled() {
            return;
        }
        if self.helpers.safepoint_flag_addr == 0 || self.helpers.safepoint_slow_path == 0 {
            return;
        }
        // Cast: x86-64 immediate encoding
        self.emit_mov_imm64(R11, self.helpers.safepoint_flag_addr as i64);
        self.emit_test_mem8_imm8(R11, 0, 0xFF);
        let no_poll = self.emit_jcc_rel32_patch(0x84); // JZ (flag clear) -> skip slow path
        self.emit_pre_safepoint_spill();
        self.emit_call_absolute(self.helpers.safepoint_slow_path);
        if self.precise_maps || self.shadow_enabled {
            self.emit_oop_map_for_safepoint();
        }
        self.patch_rel32_to_here(no_poll);
    }

    /// Method-entry variant of [`Self::emit_safepoint_poll`], called once
    /// from the END of [`Self::emit_prologue`].
    ///
    /// `emit_prologue` runs before `compile_bytecode`'s per-instruction loop
    /// ever assigns `self.cur_bc_pc` (see `compile`'s call order), so at
    /// this point `cur_bc_pc` still holds its `Compiler::new` default of
    /// `0` — the SAME value a genuine safepoint at the method's first real
    /// bytecode instruction (bci 0, a very common shape: a constructor or
    /// method that opens with `new`/an `invoke`) would use. Under
    /// `precise_maps`, both `emit_pre_safepoint_spill` and
    /// `emit_oop_map_for_safepoint` key their bookkeeping off `cur_bc_pc`
    /// (the frame's sp-id slot store and the pushed `OopMapEntry::
    /// bytecode_pc` respectively) — recording the prologue poll under the
    /// SAME bci as a real bci-0 safepoint would let the GC's sp-id-keyed
    /// oop-map lookup for a thread parked at ONE of the two safepoints
    /// match the OTHER one's (differently-shaped) frame-slot list, which
    /// could under-report live oops.
    ///
    /// `native_pc_offset` (the primary, always-unique-per-call-site key —
    /// see its doc on [`crate::OopMapEntry`]) does not collide here, only
    /// the auxiliary `bytecode_pc` cross-check does. Sidestepping it costs
    /// nothing: swap `cur_bc_pc` to `u32::MAX` (no real method's bytecode
    /// is anywhere near 4 GiB, so this can never equal a genuine bci) for
    /// the duration of the poll, then restore the saved value so the
    /// upcoming bytecode loop starts from its expected `0`.
    pub(super) fn emit_safepoint_poll_prologue(&mut self) {
        let saved_pc = self.cur_bc_pc;
        self.cur_bc_pc = ENTRY_POLL_BC_PC;
        self.emit_safepoint_poll();
        self.cur_bc_pc = saved_pc;
    }

    /// The live oop LOCALS at the current safepoint, as a bitmask over JVM
    /// local slots — or `None` when this safepoint's local state is unknown
    /// and no precise claim may be made about it.
    ///
    /// Every reader of `local_oop_masks` / `local_oop_reached` must go through
    /// here, because the METHOD-ENTRY poll is at `ENTRY_POLL_BC_PC` — a
    /// synthetic pc chosen (see [`Self::emit_safepoint_poll_prologue`]) so it
    /// cannot collide with bci 0 in the sp-id-keyed lookup. That choice is
    /// right, but `local_oop_reached.get(ENTRY_POLL_BC_PC)` is `None`, so the
    /// entry poll read as "the dataflow never reached here" and its map was
    /// pushed with `moving_young_coverage_complete: false`.
    ///
    /// The consequence was not local to the entry poll. `fully_shadow_covered`
    /// ANDs that flag over every safepoint of a method, so ONE such entry made
    /// it false for **every method the fast tier ever compiled**, which through
    /// the OSR fallback refused relocation on 725 of 759 collections of
    /// `TestKillProcessWhileWriting` — the
    /// `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821.md`
    /// residual. Measured with `CRATONVM_DBG_OOPCOV=1`: every uncovered method
    /// reported exactly `shadow_missing_pcs=[4294967295]`.
    ///
    /// The entry state is knowable and is not a guess: the prologue has just
    /// stored every incoming argument to its home (this poll is emitted from
    /// the END of `emit_prologue`), the operand stack is empty, and the live
    /// oops are exactly the reference parameters — `param_oop_mask`, the same
    /// value that seeds the dataflow at bci 0.
    #[inline]
    pub(super) fn local_oop_mask_at_current_pc(&self) -> Option<u64> {
        if self.cur_bc_pc == ENTRY_POLL_BC_PC {
            return Some(self.param_oop_mask);
        }
        if !self
            .local_oop_reached
            .get(self.cur_bc_pc)
            .copied()
            .unwrap_or(false)
        {
            return None;
        }
        self.local_oop_masks.get(self.cur_bc_pc).copied()
    }

    /// A direct self-call may omit the blind all-GPR spill when this method's
    /// exact call-site state proves every surviving operand is already visible
    /// in a canonical frame slot.
    /// The callee prologue canonicalizes its arguments before it can reach a GC
    /// safepoint; the caller still publishes its precise oop-map id.
    ///
    /// # The local test is reference-only (arch-2026-07-26 R1)
    ///
    /// This used to fail closed on `local_assignments.iter().any(Option::is_some)`
    /// — *any* register-homed local at all. That made the register allocator
    /// actively counter-productive on the workload the elision exists for: give
    /// `int fib(int n)` a register home and both recursive call sites go from
    /// `emit_safepoint_metadata_only` (2 instructions) to the full
    /// `emit_pre_safepoint_spill` — a per-local publish plus the 14-store blind
    /// full-GPR spill, twice per invocation.
    ///
    /// The test is now `no_reference_in_registers()`: the elision is refused
    /// only when a register-homed local can actually hold an **object
    /// reference**. Verified rather than assumed, on the merged tree:
    ///
    /// 1. **The GC scan is stack-only.** `OopMapEntry` carries
    ///    `frame_slot_offsets` and nothing else — the `reg_oops` register bitmap
    ///    is a TODO with no field, no producer and no consumer anywhere in the
    ///    workspace. A primitive's frame slot is therefore never *read* as a
    ///    root; and the conservative `[scanner_sp, entry_sp)` walk re-validates
    ///    every qword through `heap.is_object_address`, so a slot left stale can
    ///    only over-retain, never under-report.
    /// 2. **The reference mask is method-wide and conservative.**
    ///    `regalloc::find_reference_locals` linearly scans the whole method and
    ///    ORs in every `aload`/`astore` (all encodings, including `wide`), so
    ///    javac's cross-scope slot reuse only makes it *more* conservative — a
    ///    slot used as a reference anywhere is a reference everywhere. Unioned
    ///    with `param_oop_mask`, which covers the one case the scan cannot see:
    ///    a reference parameter the method never loads.
    /// 3. **Nothing reads a primitive's slot back.**
    ///    `emit_post_safepoint_reload` walks `local_oop_masks[pc]` — oops only —
    ///    so the store this elides has no paired load.
    /// 4. **Deopt and precise exception frames read the register, not the slot.**
    ///    `build_and_record_deopt_point` prefers `FrameValue::Register` /
    ///    `RegisterLong` / `RegisterRef` over the slot descriptor for a
    ///    register-homed local, and the frame-deopt stub fills
    ///    `deopt::SavedRegisters` with the whole GPR file. Reconstruction never
    ///    consults the unpublished slot.
    /// 5. **Locals `>= 64` cannot be register-homed at all** (`color_graph` caps
    ///    at 64 and hands out `None` above it), so a zero
    ///    `register_homed_reference_locals` really does mean "no register-homed
    ///    local can hold an oop" — there is no unrepresented tail.
    ///
    /// Consequently the R2 invariant ("the oop map's slots must be a subset of
    /// what the call site keeps current") holds here *by construction*: when the
    /// predicate passes, every oop local is frame-homed, so its canonical slot —
    /// which is exactly what `emit_oop_map_for_safepoint` advertises — was
    /// written at its `astore` and is authoritative. Register-homed *reference*
    /// locals are unchanged: they still fail the predicate and are still spilled.
    ///
    /// When `safepoint_publish` is `None` (the legacy `compile` test wrapper and
    /// the OSR artifact path, which do not build a plan) this falls back to the
    /// old all-or-nothing test, so those paths are byte-identical.
    ///
    /// Moving-young formerly disabled this optimization unconditionally. That
    /// was necessary for frames containing references because the collector
    /// suppresses its conservative JIT-frame scan and must be able to rewrite
    /// every root. It is unnecessary for an exactly proven oop-free frame:
    /// `moving_young_safepoint_coverage_complete` certifies the analysis, and an
    /// empty `collect_live_oop_homes` proves the empty precise map is complete.
    /// Any reference home or incomplete analysis still fails closed to the full
    /// spill + shadow publication.
    pub(super) fn can_elide_self_call_register_spill(&self) -> bool {
        self.call_spill_elision_core(true)
    }

    /// Shared body of [`Self::can_elide_self_call_register_spill`] and
    /// [`Self::can_elide_direct_call_register_spill`].
    ///
    /// `strict_survivors` is the ONE difference, and it is why this is a
    /// parameter rather than a copy. The self-call form requires every operand-
    /// stack survivor to be `StackSlot::Frame`. That is stronger than the
    /// mechanism needs, and it is stronger in the same way the predicate's own
    /// history records for LOCALS: it used to fail closed on *any* register-
    /// homed local and was narrowed to register-homed *reference* locals once
    /// it was shown that a primitive's slot is never read as a root.
    ///
    /// A survivor in a `CalleeSaved` register is preserved across the `CALL` by
    /// the callee's own prologue/epilogue (`LOCAL_REGS` is callee-saved on both
    /// ABIs), and its saved copy lives inside the callee's frame where the
    /// conservative `[scanner_sp, entry_sp)` walk reads it. Under the moving
    /// proof `collect_live_oop_homes()` must be empty anyway, so no oop is
    /// riding in one. A survivor in `Scratch`/`Xmm` is a different matter — the
    /// call CLOBBERS those registers, which is why `flush_scratch_registers`
    /// exists and why `emit_pre_safepoint_spill` runs it — so the relaxed form
    /// still refuses outright when one is present, rather than eliding the
    /// flush that keeps the value alive.
    ///
    /// `probes/CallArgCostProbe.java`'s arms are the shape this unlocks:
    /// `s += int1(i)` leaves the `long` accumulator on the operand stack across
    /// the invoke, in a callee-saved register. Strict survivors refused every
    /// one of them (`direct-call spill: elided=0 frame-not-clean=6`).
    fn call_spill_elision_core(&self, strict_survivors: bool) -> bool {
        let any_reference_local_in_a_register =
            reference_local_in_register(self.safepoint_publish.as_ref(), &self.local_assignments);
        if full_self_call_spill_requested() {
            return false;
        }
        if !self.precise_maps {
            if !strict_survivors {
                crate::metrics::note_call_spill(crate::metrics::CALL_SPILL_NO_PRECISE_MAPS);
            }
            return false;
        }
        if any_reference_local_in_a_register {
            if !strict_survivors {
                crate::metrics::note_call_spill(crate::metrics::CALL_SPILL_REF_LOCAL_IN_REG);
            }
            return false;
        }
        if self.stack.len() != self.stack_oop_marks.len() || !self.stack_oop_marks_exact {
            if !strict_survivors {
                crate::metrics::note_call_spill(crate::metrics::CALL_SPILL_MARKS_INEXACT);
            }
            return false;
        }

        // At this bytecode boundary the invoke arguments have already been
        // popped and staged in ABI argument registers. The callee prologue
        // canonicalizes those arguments before it can safepoint. Requiring all
        // values that survive in the caller to be frame-resident means the
        // conservative frame walk sees them regardless of their oop tags; any
        // register/XMM home fails closed to the SB-CRASH-04 full spill.
        let survivors_ok = if strict_survivors {
            self.stack
                .iter()
                .all(|slot| matches!(slot, StackSlot::Frame(_)))
        } else {
            !self
                .stack
                .iter()
                .any(|slot| matches!(slot, StackSlot::Scratch(_) | StackSlot::Xmm(_)))
        };
        if !survivors_ok {
            if !strict_survivors {
                crate::metrics::note_call_spill(crate::metrics::CALL_SPILL_SURVIVOR_IN_SCRATCH);
            }
            return false;
        }

        // `self_call_moving_proof_enabled()` rather than `moving_young_enabled()`:
        // the paired emitter `emit_safepoint_metadata_only` reads the SAME
        // predicate and fails the compile closed if the two ever disagree, so
        // they must move together. Default-identical to the old expression.
        if self.shadow_enabled || self_call_moving_proof_enabled() {
            let ok = moving_oop_free_self_call_is_publishable(
                self_call_moving_proof_enabled(),
                self.moving_young_safepoint_coverage_complete(),
                self.collect_live_oop_homes().len(),
            );
            if !ok && !strict_survivors {
                crate::metrics::note_call_spill(crate::metrics::CALL_SPILL_MOVING_UNPUBLISHABLE);
            }
            return ok;
        }
        true
    }

    /// [`Self::can_elide_self_call_register_spill`], generalised to a DIRECT
    /// call to a compiled callee that is not this method.
    ///
    /// Everything the self-call predicate proves is about THIS frame — no
    /// register-homed reference local, exact operand-stack oop marks, every
    /// surviving stack value frame-resident, and (under moving-young) a
    /// complete analysis over an empty live-oop set. None of it depends on
    /// which compiled method is called, so it transfers unchanged.
    ///
    /// What does NOT transfer is the ARGUMENTS. At the `CALL` they are staged
    /// in ABI registers, so a reference argument is a live oop with no frame
    /// home that the caller-frame proof cannot see. Two answers, selected by
    /// [`call_spill_elision_mode`]:
    ///
    ///  * mode 1 (default) — refuse the elision outright if any argument is a
    ///    reference. Conservative and needs no assumption about the callee.
    ///  * mode 2 (`args`) — admit them when `service_args_base` is `Some`, i.e.
    ///    `reserve_direct_call_service_slots` copied every argument into a
    ///    contiguous frame range for the cold callee-sentinel service. That
    ///    range lives inside `[scanner_sp, entry_sp)`, so the conservative walk
    ///    reads it, and the argument oop is frame-resident at the `CALL` after
    ///    all.
    ///
    /// Fails closed in every uncertain case, and the whole thing is off under
    /// `CRATONVM_JIT_CALL_SPILL_ELISION=0`.
    pub(super) fn can_elide_direct_call_register_spill(
        &self,
        arg_oops: &[bool],
        args_frame_resident: bool,
        min_mode: u8,
    ) -> bool {
        let mode = call_spill_elision_mode();
        if mode == 0 || mode < min_mode {
            return false;
        }
        if !self.call_spill_elision_core(false) {
            // The core already counted WHICH clause refused.
            return false;
        }
        if arg_oops.iter().any(|&o| o) && (mode < 2 || !args_frame_resident) {
            crate::metrics::note_call_spill(crate::metrics::CALL_SPILL_OOP_ARG);
            return false;
        }
        crate::metrics::note_call_spill(crate::metrics::CALL_SPILL_ELIDED);
        true
    }

    /// Return whether the shadow-stack push can prove it will publish every
    /// live oop for the current safepoint. Any `false` result is a correctness
    /// signal to the GC: if this frame is live here, moving-young must divert to
    /// the non-moving sweep for that cycle.
    fn moving_young_safepoint_coverage_complete(&self) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        if !moving_young_enabled() || self.failed {
            shadow_incomplete_cause::GATE_OFF_OR_FAILED.fetch_add(1, Relaxed);
            return false;
        }
        if self.stack.len() != self.stack_oop_marks.len() {
            shadow_incomplete_cause::MARK_VECTOR_DESYNC.fetch_add(1, Relaxed);
            return false;
        }
        if !self.stack.is_empty() && !self.stack_oop_marks_exact {
            shadow_incomplete_cause::MARKS_INEXACT.fetch_add(1, Relaxed);
            return false;
        }
        for (slot, &is_oop) in self.stack.iter().zip(self.stack_oop_marks.iter()) {
            if is_oop && matches!(slot, StackSlot::Scratch(_) | StackSlot::Xmm(_)) {
                shadow_incomplete_cause::OOP_IN_SCRATCH_OR_XMM.fetch_add(1, Relaxed);
                return false;
            }
        }
        if self.num_locals > 64 {
            shadow_incomplete_cause::TOO_MANY_LOCALS.fetch_add(1, Relaxed);
            return false;
        }
        // A live splice's callee locals are published by `collect_live_oop_homes`
        // only when its dataflow can classify them. When it cannot, this frame
        // holds references in slots nothing rewrites, so the safepoint must not
        // claim complete coverage -- which is what keeps the collector on its
        // conservative (pinning) sweep for the cycle.
        for scope in &self.inline_oop_scopes {
            if scope.mask_at_cur().is_none() {
                shadow_incomplete_cause::INLINE_SCOPE_UNMAPPED.fetch_add(1, Relaxed);
                return false;
            }
        }
        if self.num_locals == 0 {
            return true;
        }
        let ok = self.local_oop_mask_at_current_pc().is_some();
        if !ok {
            shadow_incomplete_cause::LOCAL_OOP_DATAFLOW_UNREACHED.fetch_add(1, Relaxed);
        }
        ok
    }

    /// Collect the homes of every live oop at the current safepoint: operand-
    /// stack entries tagged as references (`stack_oop_marks`) plus oop locals
    /// (`local_oop_masks[cur_bc_pc]`). XMM operand entries are skipped (they
    /// hold FP data, never references). Returns the homes in push order.
    fn collect_live_oop_homes(&self) -> Vec<ShadowHome> {
        let mut homes: Vec<ShadowHome> = Vec::new();
        // B-K kafka fix: publish ONLY the genuinely register-invisible oops —
        // operand-stack reference entries that live in a REGISTER across the
        // call. Everything else is already covered, so re-publishing it only
        // over-pins (the bt18 @ small-heap OOM):
        //   * operand-stack Frame slots are on the stack → the conservative scan
        //     `scan_active_jit_frames` already finds them;
        //   * oop LOCALS are flushed to their canonical frame slots by
        //     `emit_pre_safepoint_spill` just above → also on the stack;
        //   * Xmm entries are FP data, never references.
        // The operand-stack Reg homes (callee-saved survive the call un-spilled;
        // caller-saved/Scratch are kept for safety) are the only ones the stack
        // scan can miss, so they are exactly the set the GC must be told about.
        // Moving young gen (`CRATONVM_MOVING_YOUNG`) requires COMPLETE coverage: a
        // Cheney copy relocates every reachable object and the conservative frame
        // scan is suppressed, so EVERY live JIT-held oop must be published here as a
        // rewritable root — operand-stack entries in frame slots AND every oop
        // local (register- or frame-resident) as well, not just the
        // register-invisible operand oops. Under the register-only shadow path
        // (`CRATONVM_SHADOW_STACK` without moving), the conservative scan still
        // covers frame slots + flushed locals, so publishing only the register
        // homes avoids the over-pin OOM the B-K kafka fix warns about.
        let complete = moving_young_enabled();
        let n = self.stack.len().min(self.stack_oop_marks.len());
        for i in 0..n {
            if !self.stack_oop_marks[i] {
                continue;
            }
            match self.stack[i] {
                StackSlot::CalleeSaved(reg) | StackSlot::Scratch(reg) => {
                    homes.push(ShadowHome::Reg(reg))
                }
                // Operand entry spilled to a frame slot: covered by the
                // conservative scan on the non-moving path, but that scan cannot
                // REWRITE it, so the moving path must publish it here. The frame
                // slot uses the same `[rbp - off]` convention as the push/reload
                // (`emit_load_local`/`emit_store_local`).
                StackSlot::Frame(off) => {
                    if complete {
                        homes.push(ShadowHome::Frame(off));
                    }
                }
                StackSlot::Xmm(_) => {}
            }
        }
        // Oop locals — every reference-typed local live at this PC, in its home
        // register (`reg_for_local`) or canonical frame slot (`local_offset`).
        // Mirrors the enumeration in `build_and_record_deopt_point`. Only under
        // moving coverage: on the non-moving path a register-local is flushed to
        // its frame slot by `emit_pre_safepoint_spill` and found conservatively.
        if complete {
            let oop_mask = self.local_oop_mask_at_current_pc().unwrap_or(0);
            for i in 0..self.num_locals {
                if i >= 64 || (oop_mask & (1u64 << i)) == 0 {
                    continue;
                }
                if let Some(r) = self.reg_for_local(i) {
                    homes.push(ShadowHome::Reg(r));
                } else {
                    homes.push(ShadowHome::Frame(self.local_offset(i)));
                }
            }
            // The STAGED INVOKE-ARGUMENT buffer. Same set the oop map's Stage 3
            // names, and for the same reason: these oops were popped off the
            // simulated operand stack before the call, so neither loop above can
            // see them.
            //
            // Naming them in the MAP is not enough. The map makes them precise
            // roots for marking; the SHADOW STACK is the rewritable channel, and
            // it is the only one `moving_young_unpublished_frame_oop_present`
            // consults — `published_shadow_values(shadow_window_from_frame(..))`,
            // with no reference to the map at all. So a staged argument that was
            // mapped but not pushed sat in the frame's spill band as a
            // movable-resident word the shadow stack never published, and the
            // band verifier correctly refused the whole collection with
            // UNPUBLISHED_FRAME_OOP.
            //
            // Measured on `bug-h2-testkillprocess-zgc-oom-at-97-percent-free`:
            // `CRATONVM_JIT_INDY_BRIDGE=0` took that reason from 28 to 5 per 76
            // collections while four other 2026-08-24 switches left it above 21,
            // because a bridged `invokedynamic` stages a capturing lambda's
            // arguments across a call that runs a bootstrap and allocates.
            // `a51077342` closed the map half of this and stopped there.
            //
            // READ, not taken: `emit_oop_map_for_safepoint` still consumes the
            // list after the call, and taking it here would silently empty the
            // map's Stage 3.
            if staged_arg_shadow_enabled() {
                for off in &self.pending_staged_arg_oops {
                    homes.push(ShadowHome::Frame(*off));
                }
            }
            // THE LOCALS OF EVERY LIVE SPLICE. A call-carrying inline body
            // keeps the callee's JVM locals in this frame's spill area and then
            // makes a real GC-capable call from inside that body; neither loop
            // above can see those slots (the marks describe operands, the mask
            // describes the ENCLOSING method's locals), so without this the
            // spliced callee's `this` was published on no rewritable channel at
            // all. See `Compiler::inline_oop_scopes`.
            for scope in &self.inline_oop_scopes {
                let mask = scope.mask_at_cur().unwrap_or(0);
                for k in 0..scope.num_locals.min(64) {
                    if mask & (1u64 << k) == 0 {
                        continue;
                    }
                    // Cast: a JVM local index times 8, added to a frame offset.
                    homes.push(ShadowHome::Frame(scope.local_base + (k as i32) * 8));
                }
            }
            // A local and an operand entry can share the same home register (or two
            // operand entries the same frame slot). Deduplicate preserving order:
            // the push and reload walk the identical list, so a duplicate would
            // only re-store the same rewritten value — harmless, but it inflates
            // shadow depth, and depth drift is a documented hazard.
            let mut seen: Vec<ShadowHome> = Vec::with_capacity(homes.len());
            homes.retain(|h| {
                if seen.contains(h) {
                    false
                } else {
                    seen.push(*h);
                    true
                }
            });
        }
        homes
    }

    /// Shadow-stack push (paired with [`Self::emit_shadow_reload`]).
    ///
    /// Emitted just before a GC-capable CALL, after args are staged. For each
    /// live oop home it stores the value onto the thread's shadow stack and
    /// bumps `top`. Uses R10 (thread ptr, from the prologue-set frame slot),
    /// R11 (running `top`), and RAX (frame-slot value temp) — all caller-saved
    /// non-argument scratch, and never a live-oop home (homes are callee-saved
    /// or frame). The live homes are recorded in `pending_shadow` for the
    /// matching reload. `pending_shadow` is cleared first so an unbalanced
    /// (no-reload) safepoint cannot hand stale homes to a later reload.
    fn emit_shadow_push(&mut self) {
        // The `helpers.get_current_thread != 0` check MUST mirror the
        // prologue's gate (`emit_prologue`'s shadow-stack block): the
        // prologue only zero-initializes `shadow_thread_slot_off` when that
        // helper is wired. Without this matching guard here, a context
        // where the helper isn't wired (e.g. the JIT unit tests' stub
        // `test_helpers()`) would load uninitialized stack garbage as if it
        // were a live `*mut JvmThread` and dereference it — see the epilogue
        // fix in `emit_epilogue` for the full incident writeup.
        if self.failed
            || !self.shadow_enabled
            || self.helpers.get_current_thread == 0
            || self.shadow_thread_slot_off == 0
        {
            shadow_incomplete_cause::PUSH_NOT_EMITTED
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.pending_shadow_coverage_complete = false;
            return;
        }
        // Bisect toggle: CRATONVM_SHADOW_NOPUSH skips the push/reload codegen
        // (keeps the prologue thread-fetch + gate flip) so the SEGV can be
        // localized to push/reload vs the rest without a rebuild.
        if shadow_nopush() {
            self.pending_shadow_coverage_complete = false;
            return;
        }
        self.pending_shadow.clear();
        self.pending_shadow_coverage_complete = self.moving_young_safepoint_coverage_complete();
        let homes = self.collect_live_oop_homes();
        if !homes.is_empty() && shadow2_diag_enabled(&self.method_label) {
            let lm = self.local_oop_mask_at_current_pc().unwrap_or(0);
            let reached = self.local_oop_mask_at_current_pc().is_some();
            eprintln!(
                "[SHADOW2] method={} pc={} stack={:?} marks={:?} local_reached={} local_mask={:#x} homes={:?}",
                self.method_label,
                self.cur_bc_pc,
                &self.stack,
                &self.stack_oop_marks,
                reached,
                lm,
                homes
            );
        }
        if homes.is_empty() {
            return;
        }
        // Lazy-prologue lever: this method genuinely publishes a register-
        // resident oop, so the prologue thread-fetch must be KEPT (not NOP'd).
        self.shadow_pushed_any = true;
        let ss_top = self.shadow_off_in_thread; // + ShadowStack::TOP_OFFSET (0)
                                                // spring-bug-10 DIAGNOSTIC (CRATONVM_SHADOW_SENTINEL): write a recognizable
                                                // NON-CANONICAL sentinel into the savebase slot UNCONDITIONALLY (before the
                                                // null-thread guard), so the matching reload's value distinguishes the three
                                                // hypotheses if it faults: 0x5151_5151_5151_5151 ⇒ the push BODY was skipped
                                                // (null thread) and the slot kept the sentinel; 0xFFFF…FFFE ⇒ an EXTERNAL
                                                // write overwrote the real top with -2; a valid buffer ptr ⇒ no corruption.
        if shadow_sentinel() && self.shadow_savebase_slot_off != 0 && !shadow_no_savebase() {
            // Cast: u64 -> i64 (same-width bit reinterpretation of a sentinel pattern)
            self.emit_mov_imm64_full(R11, 0x5151_5151_5151_5151u64 as i64);
            self.emit_store_local(self.shadow_savebase_slot_off, R11);
        }
        // R10 = thread (cached in the prologue-set frame slot); R11 = shadow top.
        self.emit_load_local(R10, self.shadow_thread_slot_off);
        // Guard: if the cached thread pointer is null (get_current_thread
        // returned null for this method), skip the whole push.
        self.emit_test_r64_r64(R10);
        let skip = self.emit_jcc_rel32_patch(0x84); // JE skip (R10 == 0)
        self.emit_mov_r64_mem_disp32(R11, R10, ss_top);
        let savebase_ok = self.shadow_savebase_slot_off != 0 && !shadow_no_savebase();
        // OVERFLOW GUARD (`ShadowStack::END_OFFSET`). Without it a push that
        // runs off the end of the 2 MiB buffer keeps storing straight through
        // the allocator arena behind it — including the `JvmThread` — until it
        // leaves the mapping ~170 MiB later. That is silent heap corruption,
        // not a bail, and it is what an unbalanced push in a hot loop actually
        // produces. LEA does not touch flags, so the bump can be undone
        // between the CMP and the branch and no scratch register beyond R11 is
        // needed (RAX may hold a staged value at this point).
        //
        // `top == end` is the legal "exactly full" state, so the test is
        // strictly-above. On a bail nothing is stored and `top` is left where
        // it was; the matching reload learns this from the tag bit set in the
        // saved-base slot below (slot addresses are 8-aligned, so bit 0 is
        // free) and skips the value-restore, which would otherwise read slots
        // this push never wrote.
        let need = (homes.len() as i32) * 8; // Cast: x86-64 disp32
        let overflow = if savebase_ok && crate::shadow_end_guard_enabled() {
            self.emit_lea_r64_mem_disp32(R11, R11, need);
            self.emit_cmp_r64_mem_disp32(R11, R10, ss_top + 8); // vs `end`
            self.emit_lea_r64_mem_disp32(R11, R11, -need); // flags preserved
            Some(self.emit_jcc_rel32_patch(0x87)) // JA → would overrun
        } else {
            None
        };
        // Save this push's base `top` so the matching reload restores from /
        // resets `top` to exactly here, immune to any intervening unbalanced
        // push that drifts `top` (spring-bug-10).
        if savebase_ok {
            self.emit_store_local(self.shadow_savebase_slot_off, R11);
        }
        for &home in &homes {
            match home {
                ShadowHome::Reg(r) => {
                    self.emit_mov_mem_disp32_r64(R11, r, 0);
                }
                ShadowHome::Frame(off) => {
                    self.emit_load_local(RAX, off);
                    self.emit_mov_mem_disp32_r64(R11, RAX, 0);
                }
            }
            self.emit_lea_r64_mem_disp32(R11, R11, 8);
        }
        // Commit new top.
        self.emit_mov_mem_disp32_r64(R10, R11, ss_top);
        if let Some(overflow) = overflow {
            let done = self.emit_jmp_rel32_patch();
            self.patch_rel32_to_here(overflow);
            // Overflow bail. R11 still holds the pre-push `top` (the LEA was
            // undone), which is exactly what the reload must restore `top` to.
            // Tag it so the reload skips the value-restore.
            self.emit_or_r64_imm8(R11, 1);
            self.emit_store_local(self.shadow_savebase_slot_off, R11);
            self.emit_shadow_overflow_note();
            self.patch_rel32_to_here(done);
        }
        self.patch_rel32_to_here(skip); // null-thread guard target
        self.pending_shadow = homes;
    }

    /// Shadow-stack reload (paired with [`Self::emit_shadow_push`]).
    ///
    /// Emitted immediately after the CALL returns. Pops each pushed slot in
    /// reverse and writes the (possibly GC-rewritten) value back into its home,
    /// so a relocated object's new address flows into the registers/slots the
    /// compiled code keeps using. Preserves RAX (the call's return value): the
    /// scratch used is R10 (thread), R11 (top), R8 (frame-slot value temp).
    /// No-op when `pending_shadow` is empty (unmatched safepoint).
    fn emit_shadow_reload(&mut self) {
        // Mirror `emit_shadow_push`'s gate — see its comment for why.
        if self.failed
            || !self.shadow_enabled
            || self.helpers.get_current_thread == 0
            || self.shadow_thread_slot_off == 0
        {
            return;
        }
        if shadow_nopush() || shadow_noreload() {
            // Still drain pending so a later reload can't consume stale homes.
            self.pending_shadow.clear();
            return;
        }
        if self.pending_shadow.is_empty() {
            return;
        }
        let homes = std::mem::take(&mut self.pending_shadow);
        let ss_top = self.shadow_off_in_thread;
        self.emit_load_local(R10, self.shadow_thread_slot_off);
        // Guard: null thread pointer (see push) → skip reload. Symmetric with
        // the push guard, so a method with a null thread is consistently
        // untracked (the push was skipped too).
        self.emit_test_r64_r64(R10);
        let skip = self.emit_jcc_rel32_patch(0x84); // JE skip (R10 == 0)

        // spring-bug-10: PINNED reload. With `CRATONVM_SHADOW_PIN` the shadow
        // oops are published as PINNED roots, so the collector NEVER moves them.
        // The home values are therefore already correct after the call — callee-
        // saved registers survive it un-clobbered, and any caller-saved operand
        // oop was spilled to a frame slot by the normal pre-call codegen (and is
        // reloaded from there by the normal post-call codegen). The shadow
        // value-restore is thus redundant, and — when the saved base is stale /
        // corrupt — it is the *cause* of the corruption (it writes a wrong buffer
        // slot into a live home register; the bisection showed NORELOAD is clean
        // but reload-with-restore hangs). So under pin we SKIP the value-restore
        // entirely and only pop `top`, and only to a *validated* base: an
        // over-high `top` merely over-scans (harmless for marking), whereas a
        // too-low `top` could drop a live root, so we never pop below a
        // validated savebase (if savebase is out of range we leave `top` for the
        // JIT-exit boundary heal `restore_jit_thread` to reset).
        if shadow_pin_codegen() {
            if self.shadow_savebase_slot_off != 0 && !shadow_no_savebase() {
                self.emit_load_local(R11, self.shadow_savebase_slot_off);
                // Strip the overflow tag (see `emit_shadow_push`). A no-op on
                // the normal path — slot addresses are 8-aligned — and on the
                // bail path it recovers the pre-push `top`, which is the value
                // this path wants anyway (it only pops, never restores).
                self.emit_and_r64_imm8(R11, -2);
                self.emit_cmp_r64_mem_disp32(R11, R10, ss_top + 16); // vs base
                let leave_lo = self.emit_jcc_rel32_patch(0x82); // JB  → leave top
                self.emit_cmp_r64_mem_disp32(R11, R10, ss_top + 8); // vs end
                let leave_hi = self.emit_jcc_rel32_patch(0x83); // JAE → leave top
                self.emit_mov_mem_disp32_r64(R10, R11, ss_top); // top = savebase
                self.patch_rel32_to_here(leave_lo);
                self.patch_rel32_to_here(leave_hi);
            }
            self.patch_rel32_to_here(skip); // null-thread guard target
            return;
        }

        // Restore from the SAVED BASE this push recorded (NOT the current
        // `top`, which an intervening unbalanced push may have drifted high —
        // reading from a drifted `top` is what faulted on an uninitialised slot,
        // the spring-bug-10 SIGSEGV). `R11 = base`; each home `i` was stored at
        // `[base + i*8]`. Resetting `top` to `base` afterwards also self-corrects
        // any drift. Works for both movable (slot holds the GC-rewritten value)
        // and pinned (slot holds the unchanged value; the restore is a harmless
        // no-op since the callee-saved home was preserved).
        let n_bytes = -(homes.len() as i32) * 8; // Cast: x86-64 disp32
                                                 // SB-CRASH-04 fix: branches to here (resolved after the commit) when the
                                                 // savebase is invalid — the reload then leaves the home registers + `top`
                                                 // untouched instead of reading an out-of-bounds healed slot.
        let mut sr_patches: Vec<usize> = Vec::new();
        // Set when the push emitted an overflow guard: the branch taken when
        // its bail tag is present, resolved to the recovery block below.
        let mut ovf_patch: Option<usize> = None;
        if self.shadow_savebase_slot_off != 0 && !shadow_no_savebase() && shadow_reload_raw() {
            // DIAGNOSTIC raw deref: load savebase and use it unvalidated, so a
            // corrupt value faults on the home-restore below (crash dump shows it).
            // The overflow tag is still stripped — a legitimate overflow bail is
            // not the corruption this mode is hunting.
            self.emit_load_local(R11, self.shadow_savebase_slot_off);
            self.emit_and_r64_imm8(R11, -2);
        } else if self.shadow_savebase_slot_off != 0 && !shadow_no_savebase() {
            // R11 = savebase (the saved pre-push `top` for this safepoint).
            self.emit_load_local(R11, self.shadow_savebase_slot_off);
            // Overflow bail protocol (see `emit_shadow_push`): bit 0 set means
            // the paired push stored nothing, so the value-restore below would
            // read slots that were never written. Take the tail block instead,
            // which only puts `top` back where the push found it.
            self.emit_test_r64_imm32(R11, 1);
            ovf_patch = Some(self.emit_jcc_rel32_patch(0x85)); // JNE → bail path
                                                               // spring-bug-10 hardening: validate savebase ∈ [base, end) BEFORE
                                                               // dereferencing it for the home-restore below. The observed SIGSEGV
                                                               // read 0xFFFF_FFFF_FFFF_FFFE from this slot — a corrupt / stale /
                                                               // uninitialised value that the unconditional deref then faulted on.
                                                               // ShadowStack layout (relative to `ss_top`): top@+0, end@+8, base@+16.
                                                               //   cmp R11, base ; jb  heal   (below base)
                                                               //   cmp R11, end  ; jae heal   (at/above end)
                                                               // On heal, fall back to popping `homes.len()` slots off the live
                                                               // `top` — never out of the backing buffer, so it cannot fault.
            self.emit_cmp_r64_mem_disp32(R11, R10, ss_top + 16); // vs base
                                                                 // SB-CRASH-04 fix: on an invalid savebase (corrupt/uninitialised —
                                                                 // the observed 0xFFFF_FFFF_FFFF_FFFE), SKIP the value-restore rather
                                                                 // than healing to `top - n*8`. The old heal read `[top - n*8]`, which
                                                                 // is OUT OF BOUNDS below `base` when the stack is shallow/empty
                                                                 // (top≈base) → it loaded an adjacent non-pointer word (the observed
                                                                 // `0x1`) and wrote it into the `this` register → SIGSEGV at the next
                                                                 // field access. Under the non-moving sweep the home registers already
                                                                 // hold the correct, un-moved object, so leaving them (and `top`)
                                                                 // untouched is correct; an over-high `top` is reset by the JIT-exit
                                                                 // boundary heal (`restore_jit_thread`).
            sr_patches.push(self.emit_jcc_rel32_patch(0x82)); // JB  (savebase < base) → skip restore
            self.emit_cmp_r64_mem_disp32(R11, R10, ss_top + 8); // vs end
            sr_patches.push(self.emit_jcc_rel32_patch(0x83)); // JAE (savebase >= end) → skip restore
        } else {
            // Fallback (savebase unavailable): old current-top behaviour.
            self.emit_mov_r64_mem_disp32(R11, R10, ss_top);
            self.emit_lea_r64_mem_disp32(R11, R11, n_bytes);
        }
        for (i, &home) in homes.iter().enumerate() {
            let disp = (i as i32) * 8; // Cast: x86-64 disp32
            match home {
                ShadowHome::Reg(r) => {
                    self.emit_mov_r64_mem_disp32(r, R11, disp);
                }
                ShadowHome::Frame(off) => {
                    self.emit_mov_r64_mem_disp32(R8, R11, disp);
                    self.emit_store_local(off, R8);
                }
            }
        }
        // Commit popped top = savebase (drift-corrected).
        self.emit_mov_mem_disp32_r64(R10, R11, ss_top);
        // SB-CRASH-04: on the valid path, jump past the invalid-savebase recovery.
        let after_pop = self.emit_jmp_rel32_patch();
        // Invalid-savebase recovery (the corrupt 0xFFFF…FFFE case): DON'T restore
        // the home registers (they already hold the correct, un-moved object under
        // the non-moving sweep — reading a healed `top - n*8` went OOB below `base`
        // and corrupted `this` with `0x1`). But still POP `top` by the pushed count,
        // clamped to `base`, so the LIFO stays balanced and cannot drift/overflow.
        for p in sr_patches.drain(..) {
            self.patch_rel32_to_here(p);
        }
        self.emit_mov_r64_mem_disp32(R11, R10, ss_top); // R11 = current top
        self.emit_lea_r64_mem_disp32(R11, R11, n_bytes); // R11 = top - n*8
        self.emit_cmp_r64_mem_disp32(R11, R10, ss_top + 16); // vs base
        let no_clamp = self.emit_jcc_rel32_patch(0x83); // JAE → R11 >= base
        self.emit_mov_r64_mem_disp32(R11, R10, ss_top + 16); // clamp R11 = base
        self.patch_rel32_to_here(no_clamp);
        self.emit_mov_mem_disp32_r64(R10, R11, ss_top); // commit top = R11
        self.patch_rel32_to_here(after_pop);
        if let Some(ovf) = ovf_patch {
            // Overflow-bail recovery. Reached only from the tagged-savebase
            // branch above, so it must not fall through from the normal path.
            let past = self.emit_jmp_rel32_patch();
            self.patch_rel32_to_here(ovf);
            self.emit_load_local(R11, self.shadow_savebase_slot_off);
            self.emit_and_r64_imm8(R11, -2); // recover the pre-push `top`
                                             // Bound it before committing: an over-high `top` over-scans
                                             // (harmless) but a wild one would widen the scan out of the buffer.
            self.emit_cmp_r64_mem_disp32(R11, R10, ss_top + 16); // vs base
            let leave_lo = self.emit_jcc_rel32_patch(0x82); // JB  → leave top
            self.emit_cmp_r64_mem_disp32(R11, R10, ss_top + 8); // vs end
            let leave_hi = self.emit_jcc_rel32_patch(0x87); // JA  → leave top
            self.emit_mov_mem_disp32_r64(R10, R11, ss_top);
            self.patch_rel32_to_here(leave_lo);
            self.patch_rel32_to_here(leave_hi);
            self.patch_rel32_to_here(past);
        }
        // DBG (CRATONVM_DBG_SHADOW_RELOAD): for a single Reg home, if the reloaded
        // value is a non-pointer (< 0x10000) — the `1`-as-`this` bug — call the log
        // helper with (thread=R10, orig-savebase=[rbp-savebase_slot], slotval=home,
        // read_addr=R11). Bad-path only: the pointer path emits just cmp+jae.
        if shadow_reload_dbg() && homes.len() == 1 && self.shadow_savebase_slot_off != 0 {
            if let ShadowHome::Reg(r) = homes[0] {
                // cmp r, 0x10000   (REX.W [+ REX.B], 0x81 /7 id).
                //
                // REX.B is CONDITIONAL. This used to be a hard-coded `0x49`
                // (REX.W|REX.B) with the comment "r is r8..r15 here", and that
                // premise is false: `collect_live_oop_homes` builds
                // `ShadowHome::Reg` from `SCRATCH_REGS` *or* `LOCAL_REGS`, and
                // `LOCAL_REGS` contains RBX(3) on every platform plus RSI(6)
                // and RDI(7) on Windows. With REX.B forced on, `r == RBX`
                // encoded `cmp r11, 0x10000` — and R11 holds the shadow-buffer
                // read address, a heap pointer, so the JAE was always taken and
                // this probe could never fire for the very homes it exists to
                // inspect. `r == RSI` encoded `cmp r14, ...` instead, firing
                // spuriously on a healthy reload.
                self.buf.emit(&cmp_r64_imm32_opcode(r));
                self.buf.emit(&0x10000i32.to_le_bytes());
                let skip2 = self.emit_jcc_rel32_patch(0x83); // JAE → skip (pointer)
                self.buf.emit_byte(0x50); // push rax (preserve call return value)
                                          // `jit_dbg_shadow_reload_log` is `extern "C"`, so the argument
                                          // registers are the platform's, not Win64's. These were hard-coded
                                          // RCX/RDX/R8/R9, which on SysV passed three of the four fields in
                                          // the wrong registers and printed garbage. Every other cross-ABI
                                          // call site in this backend goes through `ARG_REGS`.
                                          //
                                          // Write order is safe on both ABIs: the sources are R10, [rbp-off],
                                          // `r` and R11, and each `ARG_REGS[i]` that could alias a later
                                          // source (`r == R8`/`R9` under Win64) is read before it is written.
                self.emit_mov_reg_reg(ARG_REGS[0], R10); // arg0 = thread
                self.emit_mov_r64_mem_disp32(ARG_REGS[1], RBP, -self.shadow_savebase_slot_off); // arg1 = orig savebase slot
                self.emit_mov_reg_reg(ARG_REGS[2], r); // arg2 = reloaded value
                self.emit_mov_reg_reg(ARG_REGS[3], R11); // arg3 = actual read address
                                                         // 0x28 on both ABIs: Win64 needs 32 bytes of shadow space, and the
                                                         // total 8 (`push rax`) + 40 = 48 keeps RSP 16-aligned either way.
                self.emit_sub_rsp_imm(0x28);
                // Cast through a raw pointer before converting the helper address to an integer.
                self.emit_call_absolute(jit_dbg_shadow_reload_log as *const () as usize);
                self.buf.emit(&[0x48, 0x83, 0xC4, 0x28]); // add rsp, 0x28
                self.buf.emit_byte(0x58); // pop rax
                self.patch_rel32_to_here(skip2);
            }
        }
        self.patch_rel32_to_here(skip); // null-thread guard target
    }

    /// T1.1.a — record an oop map at the current native PC for the
    /// live frame slots that hold object references.
    ///
    /// Call this *immediately after* any helper call that may trigger
    /// GC (object allocation, method dispatch, array allocation). The
    /// `self.buf.len()` at the time of this call is the return PC of
    /// the call — the exact PC the GC walker will look up in the
    /// finalized oop map.
    ///
    /// Empty maps (no oops live at the call) are skipped to keep the
    /// per-method oop map size bounded: GC falls back to conservative
    /// scanning for that frame, which produces the same correct
    /// result.
    ///
    /// `extra_popped` is the number of oop-typed stack entries the
    /// caller has already popped *for the call itself* but that would
    /// otherwise still be live if the call could return. For most
    /// alloc helpers this is 0 (they take primitive arguments). For
    /// `invoke*` the caller pops `this + args` before this call and
    /// passes `0` here because those entries are consumed.
    /// `CRATONVM_JIT_OOPMAP_COVERAGE_PRESENCE_ONLY=1` — count a safepoint as
    /// mapped even when its map is known to have dropped a live oop, i.e. the
    /// pre-fix accounting behind `fully_oop_covered`.
    ///
    /// Kept so the change is measurable in one binary: the fix can only ever
    /// make coverage claims RARER, and the question it opens is how much
    /// precise-only suppression it costs on workloads that were relying on it.
    fn oopmap_presence_only() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_OOPMAP_COVERAGE_PRESENCE_ONLY")
                .is_some()
        })
    }

    pub(super) fn emit_oop_map_for_safepoint(&mut self) {
        // Defensive: if the compiler is already in a failed state,
        // don't emit bogus maps.
        if self.failed {
            return;
        }
        // Shadow-stack precise roots — reload every oop pushed by the matching
        // `emit_shadow_push` from its (possibly GC-rewritten) shadow slot back
        // into its home register/frame slot, then pop. Emitted right after the
        // call returns (before the return value is consumed below). No-op when
        // the shadow gate is off or no oops were pushed at this safepoint.
        self.emit_shadow_reload();
        // Stage 1 (precise oop maps) — the lockstep invariant
        // `stack.len() == stack_oop_marks.len()` now holds continuously:
        // every push goes through `stack_push`/`push_stack` (which pair
        // both vectors) and the dup/swap/merge-reconstruct paths rebuild
        // marks in step. Assert it in debug builds. The release-mode pad
        // below is retained purely as a defensive backstop: should any
        // future un-instrumented `self.stack.push` re-introduce a desync,
        // padding with `false` (non-oop) stays SOUND because
        // `conservative_roots::scan_one_frame_precise` also conservatively
        // sweeps the frame region — but a desync would silently degrade
        // precision (and is unsafe for the *moving* path), so the assert
        // is the real guard.
        if self.stack.len() != self.stack_oop_marks.len() {
            self.stack_oop_marks_exact = false;
        }
        debug_assert_eq!(
            self.stack.len(),
            self.stack_oop_marks.len(),
            "stack/oop-marks desync at safepoint (native_pc={}): every \
             self.stack growth must go through stack_push/push_stack",
            self.buf.pos(),
        );
        while self.stack_oop_marks.len() < self.stack.len() {
            self.stack_oop_marks.push(false);
        }
        // If marks is somehow longer than stack (pop desync), truncate.
        self.stack_oop_marks.truncate(self.stack.len());

        // Consume the bound captured by the paired `emit_pre_safepoint_spill`.
        // Taking it (rather than copying) means a map emitted without a paired
        // spill records `0` = "unknown" and the GC scans conservatively, which
        // is the fail-closed direction.
        let live_frame_hi = std::mem::take(&mut self.pending_live_frame_hi);
        let native_pc = self.buf.pos() as u32; // Cast: x86-64 immediate encoding
        let mut slots: Vec<i16> = Vec::new();
        // Whether this safepoint's map is known to be INCOMPLETE — some live
        // oop could not be recorded.
        //
        // MEASURED INERT on every workload run so far (probe and ntru, all
        // three collectors): this never fires there, and the arms with and
        // without it are byte-identical. It is fail-closed hardening for drops
        // that ARE unsound whenever they happen — it is NOT the explanation for
        // the never-mapped operand-spill slots those runs report. Those are
        // staged invoke-arguments, which were never in this vocabulary to be
        // dropped from. See
        // `bug-oop-map-coverage-bit-is-presence-not-completeness-20260820.md`. Every `continue`/failed-`if let` below is
        // a silent omission, and until this existed none of them reached
        // `fully_oop_covered`, which tests only that each safepoint produced AN
        // entry (`safepoint_pcs ⊆ mapped_safepoint_pcs`). A safepoint whose map
        // dropped every oop still counted as mapped, and the runtime then spent
        // that claim to skip the conservative backstop.
        //
        // Seeded from the mark vector's own exactness. `stack_oop_marks_exact`
        // is already trusted to veto a spill elision
        // (`can_elide_self_call_register_spill`) and the shadow publication; it
        // was not consulted here. When it is false the padding a few lines above
        // filled the marks with `false`, i.e. "not an oop" for entries nobody
        // classified — sound only because a conservative sweep follows, which is
        // exactly what the coverage claim suppresses.
        let mut map_incomplete = !self.stack.is_empty() && !self.stack_oop_marks_exact;
        if map_incomplete {
            map_incomplete_cause::MARKS_INEXACT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let n = self.stack.len();
        for i in 0..n {
            if !self.stack_oop_marks[i] {
                continue;
            }
            match self.stack[i] {
                StackSlot::Frame(off) => match i16::try_from(off) {
                    Ok(i16_off) => slots.push(i16_off),
                    // A frame deeper than i16 from `rbp`. Rare, and silent.
                    Err(_) => {
                        map_incomplete = true;
                        map_incomplete_cause::STACK_OFF_TOO_DEEP
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                },
                // An oop operand that is still register/scratch/xmm resident at
                // the safepoint. There was no `else` arm here: the value is live,
                // the map does not name it, and nothing recorded that.
                _ => {
                    map_incomplete = true;
                    map_incomplete_cause::OOP_STILL_IN_REGISTER
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
        // Stage 2 (precise oop maps) — add the canonical frame slots of local
        // variables that hold object references at this safepoint, per the
        // forward "must be oop" dataflow. For register-resident locals this is
        // exactly the slot `emit_pre_safepoint_spill` flushed the live value to
        // just before the call; for memory-resident locals it is where the
        // value always lives. The GC root walker then has precise, updatable
        // coverage of every live oop local (not just operand-stack temporaries).
        // Sound on the default path regardless of dataflow precision: the
        // consumer re-validates each slot via `heap.is_object_address`.
        if !self.local_oop_masks.is_empty() {
            // Via the shared accessor: the METHOD-ENTRY poll must name its
            // reference parameters here too, or `moving_young_coverage_complete`
            // (which now claims coverage for it) would be claiming coverage of
            // a map that names nothing. See `local_oop_mask_at_current_pc`.
            //
            // A `None` here is a SILENT omission -- no slots, no cause, and the
            // map still ships claiming complete coverage. Counted (and only
            // counted) so the population can be priced before anything fails
            // closed on it: see `map_incomplete_cause::LOCAL_MASK_UNREACHED`.
            if self.local_oop_mask_at_current_pc().is_none() {
                map_incomplete_cause::LOCAL_MASK_UNREACHED
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(mut mask) = self.local_oop_mask_at_current_pc() {
                while mask != 0 {
                    // Cast: count/index to usize
                    let k = mask.trailing_zeros() as usize;
                    mask &= mask - 1; // clear lowest set bit
                    let off = self.local_offset(k);
                    match i16::try_from(off) {
                        Ok(i16_off) => {
                            if !slots.contains(&i16_off) {
                                slots.push(i16_off);
                            }
                        }
                        Err(_) => {
                            map_incomplete = true;
                            map_incomplete_cause::LOCAL_OFF_TOO_DEEP
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }
            }
        }
        // Stage 3 — the STAGED INVOKE-ARGUMENT buffer. These oops were popped
        // off the simulated operand stack before the call, so neither loop above
        // can see them; without this they were covered only by the conservative
        // bound this safepoint publishes, while the method still claimed full
        // precise coverage. Naming them here makes them precise roots, which
        // also means a moving collection REWRITES them rather than merely
        // marking them — the property the claim is actually spent on.
        //
        // Taken, not copied, so a staging site that emits no map cannot leak
        // into a later safepoint (same discipline as `live_frame_hi`).
        for off in std::mem::take(&mut self.pending_staged_arg_oops) {
            match i16::try_from(off) {
                Ok(i16_off) => {
                    if !slots.contains(&i16_off) {
                        slots.push(i16_off);
                    }
                }
                Err(_) => {
                    map_incomplete = true;
                    map_incomplete_cause::STAGED_ARG_OFF_TOO_DEEP
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
        // A reference staged somewhere no map can name it (native-ABI outgoing
        // args, direct-call service slots, inlined-callee parameter locals).
        // Fail closed.
        if std::mem::take(&mut self.pending_staged_args_unmapped) {
            map_incomplete = true;
            map_incomplete_cause::STAGED_ARG_UNMAPPABLE
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        // Stage 3b -- THE LOCALS OF EVERY LIVE SPLICE. The same set
        // `collect_live_oop_homes` publishes, named here for the same reason
        // Stage 3 names staged arguments: the shadow stack is the channel a
        // parked mutator remaps itself through, the map is what
        // `remap_active_jit_frames` rewrites, and a moving cycle needs BOTH.
        // A scope that cannot classify its locals fails the safepoint closed.
        for scope in &self.inline_oop_scopes {
            match scope.mask_at_cur() {
                Some(mask) => {
                    for k in 0..scope.num_locals.min(64) {
                        if mask & (1u64 << k) == 0 {
                            continue;
                        }
                        // Cast: a JVM local index times 8, plus a frame offset.
                        let off = scope.local_base + (k as i32) * 8;
                        match i16::try_from(off) {
                            Ok(i16_off) => {
                                if !slots.contains(&i16_off) {
                                    slots.push(i16_off);
                                }
                            }
                            Err(_) => {
                                map_incomplete = true;
                                map_incomplete_cause::INLINE_LOCAL_UNMAPPABLE
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                    }
                }
                None => {
                    map_incomplete = true;
                    map_incomplete_cause::INLINE_LOCAL_UNMAPPABLE
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }

        // Stage A.2 (precise oop maps, B-K fix) — under the precise gate, record
        // an entry for EVERY safepoint, including ones with no live oops (empty
        // `slots`). The default path keeps skipping empties (smaller metadata,
        // GC falls back to the conservative sweep there). The empty entries make
        // every safepoint's sp-id resolve to a definitive map, which is the
        // precondition for `fully_oop_covered`: without them, a safepoint with
        // zero live oops would look "un-mapped" and wrongly break coverage.
        //
        // A safepoint that LOST an oop above still pushes its entry — the entry
        // is what makes the sp-id resolve, and the recorded slots are still
        // worth rewriting — but it is NOT added to `mapped_safepoint_pcs`, so
        // `fully_oop_covered` goes false for the whole method and the collector
        // keeps its conservative backstop. This is the same fail-closed
        // direction this function already takes for a missing paired spill
        // (`live_frame_hi` = 0 = "unknown, scan conservatively").
        //
        // `mapped_safepoint_pcs` feeds `fully_oop_covered` and nothing else, so
        // withholding a pc cannot affect anything at run time.
        //
        // `CRATONVM_JIT_OOPMAP_COVERAGE_PRESENCE_ONLY=1` restores the old
        // presence-only accounting so the difference is an A/B in one binary.
        let push_map = !slots.is_empty() || self.precise_maps;
        if push_map {
            if self.precise_maps && (!map_incomplete || Self::oopmap_presence_only()) {
                // Cast: bytecode/native offset to u32 (non-negative, fits)
                self.mapped_safepoint_pcs.insert(self.cur_bc_pc as u32);
            }
            self.oop_maps.push(crate::OopMapEntry {
                native_pc_offset: native_pc,
                // Stage 3 — tag with the safepoint's bytecode PC so the GC root
                // walker can match the value the JIT stored into the sp-id slot.
                bytecode_pc: self.cur_bc_pc as u32, // Cast: bytecode PC fits u32
                frame_slot_offsets: slots,
                // `pending_shadow_coverage_complete` AND `!map_incomplete`.
                //
                // The two flags answer different halves of the same question
                // and only the first was reaching the collector.
                // `moving_young_safepoint_coverage_complete` runs back in
                // `emit_shadow_push`, BEFORE this function builds the slot
                // list, so it cannot see what building it discovers: an oop
                // still in a register, a stack/local/staged-arg offset that
                // does not fit `i16`, an inline local whose offset does not
                // either. Those set `map_incomplete` here, and `map_incomplete`
                // fed exactly one consumer — `mapped_safepoint_pcs`, i.e.
                // `fully_oop_covered`, i.e. whether the collector keeps its
                // CONSERVATIVE backstop.
                //
                // That was sound while the backstop was the whole story: a
                // conservative sweep still MARKS an oop the map missed, so
                // nothing is lost. Relocation needs more than marking — it
                // needs the slot REWRITTEN, and a conservative scan cannot
                // rewrite. So a map this function already knows to be short
                // was still published as complete coverage, and
                // `remap_one_jit_frame` rewrote only what it named and left
                // the rest pointing into from-space.
                //
                // Measured on `String.substring(II)` (safepoint 41, live band
                // `off<120`): the map named one slot — the `this` parameter
                // home — while offsets 88 and 112 inside that band held the
                // same live reference and kept their pre-move addresses. That
                // is the H2 `TestRandomMapOps` corruption, and the same shape
                // appears in `String.substring(I)` and in ordinary application
                // frames.
                //
                // Fail-closed and one-directional: this can only turn a `true`
                // into a `false`, never the reverse. The cost is that a cycle
                // reaching such a safepoint declines to relocate and the arena
                // keeps its fragmentation — which is the trade
                // `CRATONVM_ZGC_RELOCATE=0` makes today, wholesale, as the
                // workaround.
                moving_young_coverage_complete: relocation_coverage_complete(
                    self.pending_shadow_coverage_complete,
                    map_incomplete,
                ),
                live_frame_hi,
            });
            self.pending_shadow_coverage_complete = false;
        }
        // Stage 4 (precise oop maps) — reload oop register-locals from their
        // (GC-updated) canonical slots after the safepoint. `native_pc` above
        // is the call's return PC and equals the position of the first reload
        // instruction (the `push` emits no code), so the GC walker rewrites the
        // frame slots at the return PC and control then falls into the reload,
        // which propagates each moved object's new address back into its
        // callee-saved register. Without this, Stage 3 updates the slot but the
        // code keeps reading the stale register → register-invisibility
        // persists. Gated off by default (no reload → byte-identical codegen).
        if self.precise_maps && !moving_young_enabled() {
            self.emit_post_safepoint_reload();
        }
    }

    /// Stage 4 (precise oop maps) — reload every oop register-local live at the
    /// current safepoint from its canonical frame slot `[rbp - local_offset(k)]`
    /// into its assigned callee-saved register.
    ///
    /// Pairs with [`Self::emit_pre_safepoint_spill`] (which flushed the live
    /// values to those slots before the call) and the GC's
    /// `remap_active_jit_frames` (which rewrote the moved ones in place during
    /// the call). Only oop locals are reloaded — primitives don't move, so
    /// their spilled slot still matches the register. RAX is never a local
    /// register, so the call's return value survives this sequence.
    fn emit_post_safepoint_reload(&mut self) {
        if self.failed || self.local_oop_masks.is_empty() {
            return;
        }
        let Some(mut mask) = self.local_oop_mask_at_current_pc() else {
            return;
        };
        while mask != 0 {
            // Cast: count/index to usize
            let k = mask.trailing_zeros() as usize;
            mask &= mask - 1; // clear lowest set bit
            if let Some(Some(reg)) = self.local_assignments.get(k).copied() {
                let off = self.local_offset(k);
                self.emit_load_local(reg, off); // reg <- [rbp - off]
            }
        }
    }

    /// Emit the shadow-stack overflow bookkeeping: bump the process-wide bail
    /// counter and, under `CRATONVM_SHADOW_OVERFLOW_DIAG`, record this method's
    /// label. RAX is pushed/popped around it because this runs on a live call
    /// site whose arguments are already staged.
    fn emit_shadow_overflow_note(&mut self) {
        // Cast through a raw pointer before converting the static's address.
        let counter =
            (&crate::SHADOW_OVERFLOW_COUNT as *const std::sync::atomic::AtomicUsize) as usize;
        self.buf.emit_byte(0x50); // push rax
        self.emit_mov_imm64_full(RAX, counter as i64); // Cast: baked address
        self.buf.emit(&[0xF0, 0x48, 0xFF, 0x00]); // lock inc qword [rax]
        if shadow_overflow_diag() {
            let label = match self.shadow_overflow_label {
                Some(p) => p,
                None => {
                    let owned: &'static str =
                        Box::leak(format!("{}\0", self.method_label).into_boxed_str());
                    let p = owned.as_ptr() as usize;
                    self.shadow_overflow_label = Some(p);
                    p
                }
            };
            let sink =
                (&crate::SHADOW_OVERFLOW_LABEL as *const std::sync::atomic::AtomicUsize) as usize;
            // Cast: baked absolute address of a leaked NUL-terminated label.
            self.emit_mov_imm64_full(RAX, label as i64);
            self.buf.emit(&[0x48, 0xA3]); // mov moffs64, rax
            self.buf.emit(&(sink as u64).to_le_bytes());
        }
        self.buf.emit_byte(0x58); // pop rax
    }
}

/// The relocation gate: may a moving collector rewrite this frame from this
/// safepoint's map alone?
///
/// Both halves must hold, and they are discovered at different times.
/// `shadow_complete` is [`Compiler::moving_young_safepoint_coverage_complete`],
/// evaluated back in `emit_shadow_push`. `map_incomplete` is what BUILDING the
/// slot list then discovers — an oop still in a register, a stack, local,
/// staged-argument or inline-local offset that does not fit `i16`.
///
/// A free function, and pure, for the same reason
/// `g1::refuse_evacuation_for_empty_publication` is: the coupling is the whole
/// content of the fix, and a predicate that lives in a struct method a hundred
/// lines from its inputs is one nobody can pin with a truth table.
fn relocation_coverage_complete(shadow_complete: bool, map_incomplete: bool) -> bool {
    shadow_complete && (!map_incomplete || !reloc_gate_on_map_incomplete())
}

/// Kill switch for the coupling above (`CRATONVM_JIT_RELOC_GATE_ON_MAP_INCOMPLETE=0`).
///
/// Default ON, because publishing a map the compiler has already judged short
/// as complete coverage is a heap-corruption bug. It is a switch rather than a
/// bare constant because the fix has a MEASURED cost: on String-heavy code
/// every cycle that meets a live compiled frame now declines to relocate
/// (`compaction_cycles` 26 -> 0, `objects_relocated` 145 -> 0 on one probe),
/// and that reaches `TestMVStoreTool` -- an already-open fragmentation OOM --
/// roughly 10x sooner (57-61 s against 581 s). Both halves of that trade
/// deserve a same-binary A/B, and the ZGC lane needs one to bisect against.
fn reloc_gate_on_map_incomplete() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_RELOC_GATE_ON_MAP_INCOMPLETE") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The truth table of the relocation gate.
    ///
    /// The `map_incomplete=true, shadow_complete=true` row is the defect this
    /// exists for: before the coupling, that row published complete coverage.
    /// `record_oop_map` had ALREADY discovered the map was short — an oop left
    /// in a register, or an offset that would not fit `i16` — and routed that
    /// knowledge only to `mapped_safepoint_pcs`, which decides whether the
    /// CONSERVATIVE backstop stays on. A conservative sweep marks what the map
    /// missed, so that was sufficient while nothing moved. Relocation must
    /// REWRITE the slot, which the backstop cannot do, so the short map went
    /// to the collector labelled complete and `remap_one_jit_frame` left every
    /// unnamed live reference pointing into from-space.
    #[test]
    fn a_short_map_cannot_claim_relocation_coverage() {
        assert!(
            !relocation_coverage_complete(true, true),
            "a map this function knows is short must not gate relocation,              however good the shadow publication was"
        );
        assert!(relocation_coverage_complete(true, false));
        assert!(!relocation_coverage_complete(false, false));
        assert!(!relocation_coverage_complete(false, true));
    }

    /// The gate is one-directional: it can only ever REMOVE a claim.
    ///
    /// Stated as a property because the failure mode that matters is someone
    /// later "simplifying" it into something that can turn a false into a
    /// true — which would hand the collector a frame nothing proved.
    #[test]
    fn the_gate_only_ever_subtracts() {
        for shadow in [false, true] {
            for incomplete in [false, true] {
                assert!(
                    !relocation_coverage_complete(shadow, incomplete) || shadow,
                    "gate turned shadow_complete=false into a claim"
                );
            }
        }
    }

    /// `map_incomplete` must actually REACH the gate.
    ///
    /// The bug was not a wrong expression, it was a value that existed, was
    /// maintained in seven places, and was never wired to the thing it should
    /// have gated. A truth table over the predicate cannot see that; this can.
    #[test]
    fn the_pushed_map_gates_on_map_incomplete() {
        let src = include_str!("safepoint.rs");
        let at = src
            .find("moving_young_coverage_complete: relocation_coverage_complete(")
            .expect("the pushed OopMapEntry must gate through relocation_coverage_complete");
        let tail = &src[at..at + 200];
        assert!(
            tail.contains("map_incomplete"),
            "the gate at the OopMapEntry push must be fed `map_incomplete`;              found instead: {tail:?}"
        );
    }

    /// Every register a `ShadowHome::Reg` can actually name must encode as
    /// ITSELF. The `CRATONVM_DBG_SHADOW_RELOAD` probe hard-coded `0x49`
    /// (REX.W|REX.B) with the comment "r is r8..r15 here"; `LOCAL_REGS`
    /// contains RBX on every platform (and RSI/RDI on Windows), so for those
    /// homes the emitted `cmp` targeted `r + 8` instead. With `r == RBX` that
    /// is R11 — the shadow-buffer read address, always a large pointer — so
    /// `JAE` was always taken and the probe could never report the
    /// non-pointer reload it exists to catch.
    #[test]
    fn cmp_r64_imm32_sets_rex_b_only_for_the_extended_half() {
        for &r in SCRATCH_REGS.iter().chain(LOCAL_REGS.iter()) {
            let [rex, opcode, modrm] = cmp_r64_imm32_opcode(r);
            assert_eq!(opcode, 0x81, "reg {r}: opcode");
            assert_eq!(modrm >> 6, 0b11, "reg {r}: mod must be register-direct");
            assert_eq!((modrm >> 3) & 7, 7, "reg {r}: /7 selects CMP");
            assert_eq!(modrm & 7, r & 7, "reg {r}: rm must be the low 3 bits");
            assert_eq!(rex & 0x48, 0x48, "reg {r}: REX.W must be set");
            // The bug: REX.B is the fourth bit of `rm`, so it must be set iff
            // the register really is r8..r15.
            assert_eq!(
                rex & 0x01,
                u8::from(r >= 8),
                "reg {r}: REX.B must track the register's high bit — setting \
                 it unconditionally re-targets the compare at r{}",
                r + 8
            );
        }
    }

    /// A staged invoke argument is published on the SHADOW stack, not only
    /// named in the oop map.
    ///
    /// The two are different channels with different consumers. The map makes a
    /// slot a precise root for MARKING; the shadow stack is the REWRITABLE
    /// channel, and it is the only one
    /// `conservative_roots::moving_young_unpublished_frame_oop_present`
    /// consults — it asks `published_shadow_values(shadow_window_from_frame(..))`
    /// and never looks at the map. So a staged argument that reached the map but
    /// not the shadow stack sat in the frame's spill band as a movable-resident
    /// word the shadow stack never published, and the band verifier correctly
    /// refused the entire collection with `UNPUBLISHED_FRAME_OOP`.
    ///
    /// `a51077342` added the map half (`emit_oop_map_for_safepoint`'s Stage 3)
    /// and stopped there. Measured consequence on
    /// `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821`:
    /// `CRATONVM_JIT_INDY_BRIDGE=0` took `compiled-frame-oop-not-published`
    /// from 28 to 5 per 76 collections where four other switches left it above
    /// 21 — a bridged `invokedynamic` stages a capturing lambda's arguments
    /// across a call that runs a bootstrap and allocates.
    ///
    /// The assertion is `shadow ⊇ map Stage 3`, which is the invariant that was
    /// violated; it deliberately does not require equality, because the shadow
    /// list also carries register homes the map has no slot for.
    #[test]
    fn a_staged_invoke_argument_is_published_on_the_shadow_stack_too() {
        // The staged buffer only has to be published under MOVING coverage —
        // that is the mode whose band verifier demands it, and the mode
        // `collect_live_oop_homes` gates its frame-slot homes on.
        crate::x64::set_moving_young_override(Some(true));

        const STAGED: [i32; 3] = [64, 72, 80];
        let mut c = staged_arg_test_compiler();
        c.pending_staged_arg_oops = STAGED.to_vec();

        let homes = c.collect_live_oop_homes();
        for off in STAGED {
            assert!(
                homes.contains(&ShadowHome::Frame(off)),
                "staged arg at [rbp-{off}] is named by the oop map's Stage 3 but \
                 was not published on the shadow stack; homes={homes:?}"
            );
        }

        // The list the map will consume must still be intact: this collection
        // READS it, and taking it here would silently empty Stage 3.
        assert_eq!(
            c.pending_staged_arg_oops, STAGED,
            "collect_live_oop_homes must not consume the staged-arg list"
        );

        crate::x64::set_moving_young_override(None);
    }

    /// The control. Non-moving coverage keeps the conservative frame scan, which
    /// already finds a staged slot on the stack, so publishing it would only
    /// over-pin — the hazard `collect_live_oop_homes` documents as the bt18
    /// small-heap OOM. Without this, the test above would pass just as well if
    /// the homes were published unconditionally.
    #[test]
    fn a_staged_invoke_argument_is_not_published_when_nothing_moves() {
        crate::x64::set_moving_young_override(Some(false));
        let mut c = staged_arg_test_compiler();
        c.pending_staged_arg_oops = vec![64];
        let homes = c.collect_live_oop_homes();
        assert!(
            !homes.contains(&ShadowHome::Frame(64)),
            "the non-moving path must not publish staged args; homes={homes:?}"
        );
        crate::x64::set_moving_young_override(None);
    }

    /// A bare `Compiler` with no locals, no operand stack and no register
    /// assignments, so `collect_live_oop_homes` returns exactly what the staged
    /// buffer contributes and nothing else can be mistaken for it.
    fn staged_arg_test_compiler() -> Compiler {
        let alloc_result = crate::regalloc::RegAllocResult {
            assignments: Vec::new(),
            xmm_assignments: Vec::new(),
            used_callee_saved: Vec::new(),
            used_xmm_regs: Vec::new(),
            block_live_in: Vec::new(),
        };
        Compiler::new(
            "staged-arg-shadow-test".to_string(),
            ExecutableBuffer::new(4096).expect("test executable buffer"),
            0,
            0,
            8,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            alloc_result,
            false,
            // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` with all-integer
            // fields, so an all-zero bit pattern is a valid value. Nothing here
            // dereferences a helper pointer.
            unsafe { std::mem::zeroed() },
            0,
            false,
            false,
            false,
            false,
            false,
            Vec::new(),
        )
    }

    /// Spot-check two concrete encodings against the ISA so the property test
    /// above cannot pass a self-consistent but wrong rule.
    #[test]
    fn cmp_r64_imm32_matches_known_encodings() {
        // 48 81 FB — cmp rbx, imm32
        assert_eq!(cmp_r64_imm32_opcode(RBX), [0x48, 0x81, 0xFB]);
        // 49 81 FC — cmp r12, imm32
        assert_eq!(cmp_r64_imm32_opcode(R12), [0x49, 0x81, 0xFC]);
    }
}

/// `CRATONVM_JIT_NO_STAGED_ARG_SHADOW=1` — stop publishing the staged
/// invoke-argument buffer on the shadow stack, restoring the state in which it
/// was named by the oop map alone.
///
/// The bisect lever for that repair, default-ON. Latched: it is a codegen
/// decision and must not change under a running process.
fn staged_arg_shadow_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_STAGED_ARG_SHADOW").is_none()
    })
}
