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

    /// `(marks_inexact, oop_in_register, stack_deep, local_deep, staged_deep,
    /// staged_unmappable)`.
    pub fn snapshot() -> [usize; 6] {
        use std::sync::atomic::Ordering::Relaxed;
        [
            MARKS_INEXACT.load(Relaxed),
            OOP_STILL_IN_REGISTER.load(Relaxed),
            STACK_OFF_TOO_DEEP.load(Relaxed),
            LOCAL_OFF_TOO_DEEP.load(Relaxed),
            STAGED_ARG_OFF_TOO_DEEP.load(Relaxed),
            STAGED_ARG_UNMAPPABLE.load(Relaxed),
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
                if sink {
                    self.emit_blind_reg_spill(|reg| ALLOC_FAST_PATH_CLOBBERS.contains(&reg));
                    self.deferred_alloc_blind_spill = true;
                } else {
                    self.emit_blind_reg_spill(|_| true);
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
        self.emit_blind_reg_spill(|reg| !ALLOC_FAST_PATH_CLOBBERS.contains(&reg));
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
        self.cur_bc_pc = u32::MAX as usize;
        self.emit_safepoint_poll();
        self.cur_bc_pc = saved_pc;
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
        let any_reference_local_in_a_register =
            reference_local_in_register(self.safepoint_publish.as_ref(), &self.local_assignments);
        if full_self_call_spill_requested()
            || !self.precise_maps
            || any_reference_local_in_a_register
            || self.stack.len() != self.stack_oop_marks.len()
            || !self.stack_oop_marks_exact
        {
            return false;
        }

        // At this bytecode boundary the invoke arguments have already been
        // popped and staged in ABI argument registers. The callee prologue
        // canonicalizes those arguments before it can safepoint. Requiring all
        // values that survive in the caller to be frame-resident means the
        // conservative frame walk sees them regardless of their oop tags; any
        // register/XMM home fails closed to the SB-CRASH-04 full spill.
        let all_survivors_frame_resident = self
            .stack
            .iter()
            .all(|slot| matches!(slot, StackSlot::Frame(_)));
        if !all_survivors_frame_resident {
            return false;
        }

        // `self_call_moving_proof_enabled()` rather than `moving_young_enabled()`:
        // the paired emitter `emit_safepoint_metadata_only` reads the SAME
        // predicate and fails the compile closed if the two ever disagree, so
        // they must move together. Default-identical to the old expression.
        if self.shadow_enabled || self_call_moving_proof_enabled() {
            return moving_oop_free_self_call_is_publishable(
                self_call_moving_proof_enabled(),
                self.moving_young_safepoint_coverage_complete(),
                self.collect_live_oop_homes().len(),
            );
        }
        true
    }

    /// Return whether the shadow-stack push can prove it will publish every
    /// live oop for the current safepoint. Any `false` result is a correctness
    /// signal to the GC: if this frame is live here, moving-young must divert to
    /// the non-moving sweep for that cycle.
    fn moving_young_safepoint_coverage_complete(&self) -> bool {
        if !moving_young_enabled() || self.failed {
            return false;
        }
        if self.stack.len() != self.stack_oop_marks.len() {
            return false;
        }
        if !self.stack.is_empty() && !self.stack_oop_marks_exact {
            return false;
        }
        for (slot, &is_oop) in self.stack.iter().zip(self.stack_oop_marks.iter()) {
            if is_oop && matches!(slot, StackSlot::Scratch(_) | StackSlot::Xmm(_)) {
                return false;
            }
        }
        if self.num_locals > 64 {
            return false;
        }
        if self.num_locals == 0 {
            return true;
        }
        self.local_oop_reached
            .get(self.cur_bc_pc)
            .copied()
            .unwrap_or(false)
            && self.local_oop_masks.get(self.cur_bc_pc).is_some()
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
            let oop_reached = self
                .local_oop_reached
                .get(self.cur_bc_pc)
                .copied()
                .unwrap_or(false);
            let oop_mask = if oop_reached {
                self.local_oop_masks
                    .get(self.cur_bc_pc)
                    .copied()
                    .unwrap_or(0)
            } else {
                0
            };
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
            let lm = self
                .local_oop_masks
                .get(self.cur_bc_pc)
                .copied()
                .unwrap_or(0);
            let reached = self
                .local_oop_reached
                .get(self.cur_bc_pc)
                .copied()
                .unwrap_or(false);
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
            let pc = self.cur_bc_pc;
            if pc < self.local_oop_masks.len()
                && self.local_oop_reached.get(pc).copied().unwrap_or(false)
            {
                let mut mask = self.local_oop_masks[pc];
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
                moving_young_coverage_complete: self.pending_shadow_coverage_complete,
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
        let pc = self.cur_bc_pc;
        if pc >= self.local_oop_masks.len()
            || !self.local_oop_reached.get(pc).copied().unwrap_or(false)
        {
            return;
        }
        let mut mask = self.local_oop_masks[pc];
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

#[cfg(test)]
mod tests {
    use super::*;

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
