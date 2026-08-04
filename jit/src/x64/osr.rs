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

/// Publish this compile's OSR **entry** metadata onto `cm`, or publish none.
///
/// Lifted out of `compile_with_param_slots` (SEAM-01). It was ~280 statements
/// in the middle of a 2,200-line function, and it is the part of that function
/// with the least margin for a misreading: it builds four vectors across
/// **three** coordinate spaces — interpreter bci, output pc, local index — and
/// moves two of them between spaces when the bytecode loop rewriter is armed.
///
/// Everything it reads is a parameter and everything it writes goes through
/// `cm`, so the extraction is mechanical; the argument list is long because the
/// dependency was long, not because the split introduced one.
///
/// Fails closed, per `docs/feature-designs/jit-osr-entry-metadata.md`: if
/// `osr_contract::check_at_publication` rejects the set, every OSR field is
/// cleared rather than published half-agreeing. `can_osr_enter` then refuses
/// every pc and the method runs to completion in the interpreter, which is
/// always valid. It clears rather than returning early because the caller still
/// has non-OSR state to publish (oop maps, deopt points, frame layout, the
/// epoch guard); an early return would turn a metadata disagreement into a
/// much larger regression than the one it prevents.
#[allow(clippy::too_many_arguments)]
pub(super) fn publish_entry_metadata(
    cm: &mut CompiledMethod,
    code: &[u8],
    code_len: usize,
    orig_code_len: usize,
    loop_xform: &Option<LoopXform>,
    osr_entry_native: Vec<i32>,
    local_assignments: &[Option<u8>],
    xmm_assignments: &[Option<u8>],
    osr_block_live_in: &[(usize, u64)],
    num_locals: usize,
    num_reg_locals: usize,
    kernel_reg_homes: bool,
    kernel_reg_homes_osr_requested: bool,
    method_label: &str,
) {
    // Store OSR metadata for On-Stack Replacement entry.
    //
    // OSR uses `osr_entry_native` rather than the branch-patch `pc_to_native`:
    // for loop headers carrying LICM-hoisted preheader code the two differ —
    // `pc_to_native[header]` points *after* the preheader (so in-loop
    // back-edges skip it) while `osr_entry_native[header]` points *before* it
    // (so a cold OSR entry runs the hoist initialisation). For every other PC
    // the two are identical.
    //
    // Pure-kernel GPR local homes: publish NO OSR entries for such a body.
    // Its non-reference locals live exclusively in callee-saved registers,
    // and the OSR trampoline's frame-seeded entry contract is exactly the
    // "OSR transition" the kernel-homes safety argument excludes. The OSR
    // pipeline compiles its own separate, memory-homed artifact
    // (`compile_osr_artifact` never requests kernel homes), so loop-hot
    // methods still get OSR service.
    //
    // Coordinate change, artifact half. `osr_entry_native` is written by the
    // emitter at the pc it is emitting, so under a bytecode rewrite it is
    // indexed by OUTPUT pc while the runtime indexes the published vector by
    // INTERPRETER bci. It cannot merely be translated on read: a bci inside a
    // transformed region has SEVERAL native offsets and picking the wrong one
    // re-runs iterations. `LoopXform::rebuild_pc_to_native` applies
    // `osr_entry_pc`'s steady-state choice pointwise and leaves the `-1`
    // sentinel wherever there is no valid image (the unrolled back-edge gap),
    // where entering compiled code is not valid at all and `can_osr_enter`
    // must refuse. The identity — the same vector, moved — when unarmed.
    let osr_entry_native = match loop_xform {
        Some(x) => {
            let mut v = x.rebuild_pc_to_native(&osr_entry_native, orig_code_len);
            // Enforce the refusal independently of who filled the vector.
            //
            // `rebuild_pc_to_native` leaves the sentinel wherever `osr_entry_pc`
            // answers `None`, so on the path where it is the sole producer this
            // loop is a no-op. It is here because it was NOT the sole producer:
            // the emitter also writes `osr_entry_native` while emitting, once
            // per copy, and the back-edge bci is written by the LAST copy. That
            // left a live entry at a bci with no steady-state image — entering
            // there resumes a "back edge next" frame at the top of a fresh body
            // and runs one extra iteration. Caught by
            // `a_rewritten_compile_publishes_osr_metadata_in_interpreter_bci_space`
            // the first time the suite was run against the wired rewriter.
            //
            // Stated as an invariant rather than a repair: after this, no bci
            // that `osr_entry_pc` refuses carries an offset, whatever produced
            // the vector.
            for (bci, slot) in v.iter_mut().enumerate() {
                if x.osr_entry_pc(bci).is_none() {
                    *slot = -1;
                }
            }
            v
        }
        None => osr_entry_native,
    };
    cm.osr_pc_to_native = if kernel_reg_homes && !kernel_reg_homes_osr_requested {
        // Method-entry kernel homes: same length, every entry -1 —
        // `can_osr_enter` refuses every pc (the method-entry body was never
        // built for trampoline entry).
        Some(vec![-1; osr_entry_native.len()])
    } else {
        // Ordinary bodies AND OSR-tier kernel-homed bodies publish real
        // entries: the OSR trampoline seeds every local into its
        // `osr_local_assignments` register (or frame slot for `None`/ref
        // locals), which for a kernel-homed body is exactly its homes.
        Some(osr_entry_native)
    };
    cm.osr_num_locals = num_locals;
    cm.osr_num_reg_locals = num_reg_locals;

    // --- OSR soundness: drop register assignments for long/double high-half
    // slots ---------------------------------------------------------------
    // A `long`/`double` JVM local at index N reserves index N+1 as its dead
    // "high half". The JIT models 64-bit values as a single register, so it
    // never reads index N+1 — but the graph-colouring allocator still hands
    // that dead slot a physical register, and freely reuses one register for
    // *several* dead high-halves AND a live local (they never interfere, so
    // colouring is legal for the running code).
    //
    // The OSR trampoline, however, copies every `jit_locals[i]` into
    // `local_assignments[i]`'s register in ascending index order. When a dead
    // high-half index shares a register with a live local at a *lower* index,
    // the trampoline's write of the high-half's garbage value (the interpreter
    // supplies 0 for the unused slot) clobbers the live local that was already
    // loaded. For a `long` loop counter this reset the counter to 0 mid-loop,
    // producing a wrong result; for a pointer-typed local it corrupts a heap
    // reference and segfaults.
    //
    // Fix: null out the OSR register assignment for every high-half slot.
    // The high-half carries no live value, so the trampoline simply spills its
    // garbage to a frame slot nobody reads — and the live local keeps its
    // register. This only touches the OSR metadata copy; the running code's
    // `reg_for_local` (which never asks for a high-half) is unaffected.
    let mut osr_local_assignments = local_assignments.to_vec();
    // ES-tdigest OSR fix: the high-half nulling and the per-PC dead mask below
    // must cover XMM-resident (float/double) locals exactly like GPR-resident
    // ones. DualPivotQuicksort.sort coalesces several disjoint-live-range
    // double locals (pivots, run temporaries) onto one XMM register; an OSR
    // entry at a PC where one of them is dead loaded the dead local's garbage
    // over the live owner's XMM value (the GPR-only mask said "safe"), so
    // Arrays.sort(double[]) silently mis-sorted / threw garbage-index AIOOBE
    // once the sort loop OSR-entered.
    let mut osr_xmm_assignments = xmm_assignments.to_vec();
    {
        // …but ONLY for a slot that is NOTHING BUT a high half.
        //
        // `wide_local_high_halves` is a whole-method scan: it marks `N+1` for
        // every `lstore N`/`dstore N` anywhere in the method. Under legal JVM
        // slot reuse the same index is frequently a live cat-1 local in a
        // DISJOINT range — `java.util.DualPivotQuicksort.mixedInsertionSort`
        // has slot 7 as `long ai`'s high half in its first region and as the
        // `int i` loop counter in the other two. Nulling that slot's OSR
        // register assignment made the trampoline seed only its FRAME slot
        // while the compiled body kept reading its REGISTER, so an OSR entry
        // into the second region ran with a garbage `i`: the insertion loop
        // `while (ai < a[--i])` walked off the front of the array and threw
        // `ArrayIndexOutOfBoundsException` with an index in the hundreds of
        // millions. `Arrays.sort(long[])` of >= 1000 elements failed on the
        // FIRST sort, every run — see
        // `docs/known-issues/jit/arrays-sort-long-osr-miscompile-20260803.md`.
        //
        // `classify_local_kinds` already draws exactly this distinction: a high
        // half that is independently accessed is `Ambiguous`, an untouched one
        // is `HighHalf`. Only the latter is safe to strip.
        //
        // The hazard the stripping exists for — a dead slot sharing a register
        // with a live local, whose seed would clobber the live owner — is still
        // covered for `Ambiguous` slots, and more precisely, by the per-entry-PC
        // dead mask built right below: at a PC where such a slot really is the
        // dead high half it is not live-in, so it lands in the blanket set and
        // is masked if (and only if) its register is genuinely shared.
        let kinds = classify_local_kinds(code, code_len, num_locals);
        let high_halves = wide_local_high_halves(code, code_len);
        for &hh in &high_halves {
            if !matches!(kinds.get(hh), Some(LocalKind::HighHalf)) {
                continue;
            }
            if hh < osr_local_assignments.len() {
                osr_local_assignments[hh] = None;
            }
            if hh < osr_xmm_assignments.len() {
                osr_xmm_assignments[hh] = None;
            }
        }
    }
    // Per-OSR-entry-PC "dead local" mask. The OSR trampoline loads locals into
    // their (graph-colouring-coalesced) registers in index order; a local that
    // is DEAD at the entry PC but shares a register with a LIVE local would
    // clobber the live one when loaded (e.g. an `int[]` arg and a later-loop
    // accumulator colour to the same callee-saved register because their live
    // ranges don't overlap — entering the first loop then reading the array
    // gets the accumulator's value, a null/garbage pointer → spurious NPE →
    // OSR deopt → back-off → the loops never sustain JIT). The previously-fixed
    // category-2 high-half clobber is one instance; this generalises it to any
    // pair of real locals. For each basic-block start PC (OSR entries are
    // loop-header block starts), mark the register-resident locals NOT live
    // there so the trampoline skips loading them, leaving each shared register
    // to its live owner.
    let reg_resident: u64 = osr_local_assignments
        .iter()
        .enumerate()
        .filter(|(_, a)| a.is_some())
        .fold(0u64, |m, (i, _)| if i < 64 { m | (1u64 << i) } else { m });
    // XMM-resident locals participate in the same graph-colouring coalescing
    // as GPR-resident ones, so they need the same dead-at-entry protection
    // (liveness tracks d/f locals at their base index via dload/dstore).
    let xmm_resident: u64 = osr_xmm_assignments
        .iter()
        .enumerate()
        .filter(|(_, a)| a.is_some())
        .fold(0u64, |m, (i, _)| if i < 64 { m | (1u64 << i) } else { m });
    // The mask must name the dead locals that are actually *hazardous*, not
    // every dead local. `osr_enter` declines any entry whose mask is non-zero
    // (a deliberate 2026-07-04 conservatism: the trampoline's skip-the-load
    // avoided clobbering the live owner, but the resulting coalesced state
    // transition was not proven safe -- see
    // docs/internal/fixed-suite-bugs/jit-osr-linux-regression-triad.md). The
    // hazard that argument rests on is *sharing*: a dead local whose register
    // is also some live local's home. A dead local that owns its register
    // outright has no coalesced state to reconstruct -- nothing reads it before
    // the loop redefines it -- so flagging it only costs OSR entries.
    //
    // The blanket form cost a lot of them. `org/h2/compress/CompressLZF.
    // compress(Ljava/nio/ByteBuffer;I[BI)I` -- the single hottest method in
    // H2's `TestFileSystem` `nioMemLZF:` case -- was refused at its main loop
    // header (`entry_pc=220`, mask `0x201`: `this` and one temporary, neither
    // sharing a register with anything live) and so never ran compiled at all
    // (2026-07-27).
    //
    // Set CRATONVM_JIT_OSR_DEAD_MASK_BLANKET=1 to restore the old
    // flag-every-dead-local behaviour.
    let blanket_dead_mask =
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_OSR_DEAD_MASK_BLANKET").is_some();
    let resident = reg_resident | xmm_resident;
    // `local <-> home register` lookup, GPR and XMM kept apart: they are
    // different register files and can never alias each other.
    let gpr_home = |i: usize| osr_local_assignments.get(i).copied().flatten();
    let xmm_home = |i: usize| osr_xmm_assignments.get(i).copied().flatten();
    let mut osr_dead_mask = vec![0u64; code_len + 1];
    for &(pc, live_in) in osr_block_live_in {
        if pc >= osr_dead_mask.len() {
            continue;
        }
        let dead = resident & !live_in;
        if blanket_dead_mask || dead == 0 {
            osr_dead_mask[pc] = dead;
            continue;
        }
        let live_resident = resident & live_in;
        let mut hazardous = 0u64;
        for i in 0..64 {
            if (dead >> i) & 1 == 0 {
                continue;
            }
            let (dg, dx) = (gpr_home(i), xmm_home(i));
            for j in 0..64 {
                if (live_resident >> j) & 1 == 0 {
                    continue;
                }
                let shares = (dg.is_some() && dg == gpr_home(j))
                    || (dx.is_some() && dx == xmm_home(j));
                if shares {
                    hazardous |= 1u64 << i;
                    break;
                }
            }
        }
        osr_dead_mask[pc] = hazardous;
    }
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_OSR_META").is_some() {
        // Report the mask that is actually published, alongside the blanket
        // "every dead register-resident local" set it is refined from, so the
        // two can be compared directly. Printing a separately recomputed
        // blanket value made this diagnostic silently disagree with the real
        // metadata once the refinement landed.
        let mut blanket: Vec<(usize, u64)> = Vec::new();
        let mut published: Vec<(usize, u64)> = Vec::new();
        for &(pc, live_in) in osr_block_live_in {
            let b = (reg_resident | xmm_resident) & !live_in;
            if b != 0 {
                blanket.push((pc, b));
            }
            let p = osr_dead_mask.get(pc).copied().unwrap_or(0);
            if p != 0 {
                published.push((pc, p));
            }
        }
        if !blanket.is_empty() || !published.is_empty() {
            eprintln!(
                "[osr-meta] gpr_resident={reg_resident:#x} xmm_resident={xmm_resident:#x} \
                 blanket_entries={blanket:x?} published_entries={published:x?} \
                 unblocked={}",
                blanket.len() - published.len()
            );
        }
    }
    // Indexed by the same interpreter bci as `osr_pc_to_native` above, so it
    // needs the same coordinate change and the same image choice — a mask
    // read for one copy while the entry jumps into another would skip loading
    // a local that IS live at the entry it actually takes. A bci with no
    // steady-state image keeps a zero mask, which is never read: the entry is
    // already refused by the `-1` in `osr_pc_to_native`.
    let osr_dead_mask = match loop_xform {
        Some(x) => {
            let mut rebuilt = vec![0u64; orig_code_len + 1];
            for (bci, slot) in rebuilt.iter_mut().enumerate() {
                if let Some(image) = x.osr_entry_pc(bci) {
                    *slot = osr_dead_mask.get(image).copied().unwrap_or(0);
                }
            }
            rebuilt
        }
        None => osr_dead_mask,
    };
    // ── The OSR entry-metadata contract ──────────────────────────────
    //
    // Everything above built four vectors in THREE different coordinate spaces
    // (interpreter bci, output pc, local index) and moved two of them between
    // spaces. `osr_contract::check_at_publication` is the one place that says,
    // in code rather than in a comment, that the results agree.
    //
    // Fail closed, per `docs/feature-designs/jit-osr-entry-metadata.md`: on a
    // violation publish NO OSR metadata at all rather than a set whose pieces
    // disagree. `can_osr_enter` then answers false everywhere and the method
    // runs to completion in the interpreter, which is always valid. Over-
    // refusal costs an optimisation; under-refusal re-runs loop iterations or
    // resumes with the wrong locals.
    //
    // The check that matters is the length of the two bci-indexed vectors:
    // `can_osr_enter_with` reads the dead mask through `.unwrap_or(0)`, so a
    // short mask reads as "no dead locals" for every bci in the tail and admits
    // entries that must be refused. Nothing downstream can notice.
    let osr_metadata_agrees = crate::osr_contract::check_at_publication(
        cm.osr_pc_to_native.as_deref().unwrap_or(&[]),
        &osr_dead_mask,
        &osr_local_assignments,
        &osr_xmm_assignments,
        num_locals,
        method_label,
    );
    //
    // Clearing the fields rather than returning early: everything below this
    // point publishes NON-OSR state (oop maps, deopt points, frame layout, the
    // epoch guard). An early return would drop all of it and turn a metadata
    // disagreement into a much larger regression than the one it prevents.
    if osr_metadata_agrees {
        cm.osr_dead_mask = Some(osr_dead_mask);
        cm.osr_local_assignments = Some(osr_local_assignments);
        cm.osr_xmm_assignments = Some(osr_xmm_assignments);
    } else {
        cm.osr_pc_to_native = None;
        cm.osr_dead_mask = None;
        cm.osr_local_assignments = None;
        cm.osr_xmm_assignments = None;
    }
}
