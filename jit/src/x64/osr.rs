// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! On-stack-replacement exit maps.
//!
//! The emitter side of OSR *exit*: a precise deopt snapshot tagged
//! `DeoptReason::OsrExit`, recorded at a loop boundary that has already been
//! vetted OSR-eligible. Entry — the trampoline and the entry-offset table — is
//! published by `compile_with_param_slots` and described by
//! `docs/feature-designs/jit-osr-entry-metadata.md`.
//!
//! Eligibility is inherited, not re-derived: a bci qualifies only if
//! `osr_entry_native[pc] >= 0`, which already excludes loop headers carrying
//! LICM-hoisted preheader code. Entering one of those at the header would run
//! the body with uninitialised hoist slots.

use super::*;

impl Compiler {
    // -----------------------------------------------------------------------
    // Deopt points, exception checks and the stub block
    // -----------------------------------------------------------------------
    //
    // Moved to `x64/deopt_stubs.rs`, including the bci-baking sites: the stub
    // block is emitted after the whole body, so the code that decides which bci
    // a stub resumes at is not at the guard it serves.


    /// deopt-osr Step 7: emit an OSR-exit map — a precise deopt snapshot tagged
    /// `DeoptReason::OsrExit` — at a loop-boundary `bci` (one of the PCs already
    /// vetted OSR-eligible, i.e. `osr_entry_native[pc] >= 0`, so it inherits the
    /// LICM-hoist rejection). EMIT-AND-DISCARD: it records the map in
    /// `deopt_points`/`deopt_boxes` + the OSR-exit PC set, but no exit path
    /// consumes it until Step 8 routes the mid-loop bail through the same deopt
    /// trampoline. Only called when `deopt_real_enabled()` (see the call site), so
    /// production builds zero OSR-exit metadata and stay byte-identical.
    pub(super) fn emit_osr_exit_map_at(&mut self, bci: usize) {
        self.emit_osr_exit_map_at_reason(bci, crate::deopt::DeoptReason::OsrExit);
    }

    /// Same snapshot machinery as `emit_osr_exit_map_at`, but lets the caller
    /// stamp the box's `DeoptReason` explicitly. FIX (2026-07-07,
    /// jit-invokedynamic-groovy-regression, THIRD-pass root cause): this
    /// snapshot machinery is shared by two call sites — the true loop-header
    /// OSR-exit (Step 7/8) and the `invokedynamic` uncommon-trap snapshot
    /// (opcode `0xba`, reason 8 = `UnreachedCode`) — but both used to hard-code
    /// `DeoptReason::OsrExit` on the box regardless of which site created it.
    /// `real_frame_deopt_resume_and_despeculate`'s de-speculation step recovers
    /// the reason FROM THE BOX (`compiled.deopt_points.find(|dp| dp.bci ==
    /// rframe.bci).map(|dp| dp.reason)`), not from the `8` baked into
    /// `deopt_stubs` — so an invokedynamic trap that fires was mis-classified
    /// as an ordinary `OsrExit` and got the count-based recompile-and-retry
    /// policy instead of `UnreachedCode`'s "give up immediately"
    /// (`MakeNotCompilable`). For Groovy's `IndyInterface`-based dynamic
    /// dispatch — where this "unreachable" trap is actually reached on
    /// essentially every call — that meant the method stayed compiled and kept
    /// re-entering the trap on every subsequent invocation, each time pushing a
    /// BRAND NEW interpreter frame via `resume_real_ir_deopt` to re-run the
    /// call from scratch. Confirmed via `CRATONVM_DBG_DEOPT`: a single failing
    /// `simpleBean()` run shows ~2000 `x64 frame-deopt entry` events (all
    /// mis-labeled `reason=OsrExit`) for one Groovy script evaluation — each
    /// one a fresh re-entry into Groovy's own script/closure class-generation
    /// machinery, which is exactly what produces "doCall duplicates another
    /// method" (Groovy's compiler observing its own generated method
    /// registered more than once). Fixed by stamping the CORRECT reason at
    /// each call site (see the two `emit_osr_exit_map_at`/
    /// `emit_osr_exit_map_at_reason` calls) so `UnreachedCode` finally reaches
    /// `recommend_action` and gets `MakeNotCompilable` on first occurrence, as
    /// `fb4a333d` always intended.
    pub(super) fn emit_osr_exit_map_at_reason(&mut self, bci: usize, reason: crate::deopt::DeoptReason) {
        let box_ptr = self.build_and_record_deopt_point(bci, reason);
        self.osr_exit_box_ptr_by_bci.insert(bci, box_ptr);
        self.osr_exit_points.push(bci);
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DEOPT").is_some() {
            // Step-7 emit-and-discard trace: confirm an exit map was recorded at
            // this OSR-vetted loop boundary (locals/stack come from the same
            // unit-tested `frame_value_for_slot` provenance as the guard path).
            // `if let` (not `.unwrap()`) keeps this debug trace panic-free —
            // `build_and_record_deopt_point` just pushed, so `last()` is Some.
            if let Some(p) = self.deopt_points.last() {
                eprintln!(
                    "[cratonvm-deopt] OSR-exit map emitted at bci={bci} \
                     (locals={}, stack={})",
                    p.frame_state.locals.len(),
                    p.frame_state.stack.len(),
                );
            }
        }
    }

    /// deopt-osr Step 8 follow-up (P4): emit a COUNTER-GATED OSR-exit trigger at a
    /// loop header — bail to the OSR-exit deopt stub (reason 7) only on the `n`-th
    /// reach, so the JIT runs ~`n` loop iterations (advancing the loop-carried
    /// locals/accumulator AND committing their per-iteration side effects) before
    /// the exit. The reconstructed frame then carries genuinely JIT-advanced state,
    /// which the interpreter's `transfer_osr_exit_into_live_frame` writes back into
    /// the live frame — the difference between a *true* OSR-exit and the
    /// unconditional-at-header trigger (which bails at iteration 0, where reject and
    /// transfer coincide).
    ///
    /// The counter is a leaked per-site `Box<i64>` (one per compiled
    /// method-with-a-loop, ONLY under `CRATONVM_OSR_EXIT_AFTER`; never in
    /// production). The sequence is transparent to the JIT's machine state at this
    /// loop-header basic-block boundary:
    ///   * RAX/RCX are saved/restored with PUSH/POP — and POP does NOT modify
    ///     EFLAGS, so the `CMP` result survives to the `JL`;
    ///   * RSP is balanced (both pops run) before EITHER exit (`JL over` /
    ///     fall-through `JMP stub`), so the stub sees the normal frame;
    ///   * EFLAGS are dead at a branch target (the JIT recomputes loop conditions),
    ///     so clobbering them here is sound.
    ///
    /// ```text
    ///   push rax                       ; 50
    ///   push rcx                       ; 51
    ///   mov  rax, &counter             ; 48 B8 imm64
    ///   mov  rcx, [rax]                ; 48 8B 08
    ///   inc  rcx                       ; 48 FF C1
    ///   mov  [rax], rcx                ; 48 89 08
    ///   cmp  rcx, n                    ; 48 81 F9 imm32
    ///   pop  rcx                       ; 59   (flags preserved)
    ///   pop  rax                       ; 58   (flags preserved)
    ///   jl   over                      ; 7C 05  (counter < n ⇒ skip the bail)
    ///   jmp  osr_exit_stub             ; E9 rel32 (patched via deopt_stubs)
    /// over:
    /// ```
    pub(super) fn emit_osr_exit_after_trigger(&mut self, pc: usize, n: usize) {
        // LEAK(intentional): per-site counter, leaked so its address outlives
        // the emitted code's baked imm64. Test-only path; bounded by emitted
        // OSR trigger sites in this test.
        let counter: *mut i64 = Box::leak(Box::new(0i64));

        self.buf.emit_byte(0x50); // push rax
        self.buf.emit_byte(0x51); // push rcx
                                  // mov rax, &counter (imm64)
        self.emit_mov_imm64_full(RAX, counter as i64); // Cast: address → imm64
        self.buf.emit(&[0x48, 0x8B, 0x08]); // mov rcx, [rax]
        self.buf.emit(&[0x48, 0xFF, 0xC1]); // inc rcx
        self.buf.emit(&[0x48, 0x89, 0x08]); // mov [rax], rcx
                                            // cmp rcx, n (imm32)
        self.buf.emit(&[0x48, 0x81, 0xF9]);
        self.buf.emit(&(n as i32).to_le_bytes()); // Cast: iteration count → imm32
        self.buf.emit_byte(0x59); // pop rcx (EFLAGS preserved)
        self.buf.emit_byte(0x58); // pop rax (EFLAGS preserved)
        self.buf.emit(&[0x7C, 0x05]); // jl over (skip the 5-byte JMP when counter < n)
        self.buf.emit_byte(0xE9); // jmp rel32 → OSR-exit stub
        let patch_off = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.deopt_stubs.push((patch_off, pc, 7)); // 7 = OSR-exit
                                                   // `over:` is the next emitted instruction.
    }
}
