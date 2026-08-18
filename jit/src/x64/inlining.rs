// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Callee inlining at the emitter level.
//!
//! `try_emit_inline` decides whether a call site chosen callee can be emitted
//! in line, and `try_emit_inline_body` walks that callee bytecode against this
//! frame. It is a second, smaller bytecode walk with a different set of rules
//! — the callee locals live in the caller spill area, its metadata is read
//! from the `InlineSite` rather than from this method tables, and anything it
//! cannot model is a refusal rather than a bail.
//!
//! Refusing here is always safe: the call site falls back to a real call.

use super::*;

impl Compiler {
    // -----------------------------------------------------------------------
    // Object headers, fields, allocation, string layout
    // -----------------------------------------------------------------------
    //
    // Moved to `x64/objects.rs`.



    /// Emit inlined callee bytecode at the given caller PC.
    ///
    /// Returns `true` if inlining succeeded, `false` to fall back to a normal call.
    /// The callee's locals are allocated in the caller's spill area so no new frame
    /// is needed. Forward branches within the callee are tracked and patched after
    /// emission. On return, the callee's result (if any) is on the caller's operand
    /// stack.
    /// Speculatively inline the callee at `pc`. On any mid-body bail this
    /// rolls back ALL speculative state — emitted machine code, the
    /// simulated operand stack, its oop-mark vector, and the spill
    /// cursor — so the caller can cleanly fall back to a normal call.
    ///
    /// Historically the `return false` bail points inside the inline
    /// interpreter reset only `next_spill_offset`; the partially-emitted
    /// callee body (and any operand-stack pops) were left in place. The
    /// caller then emitted a *second*, full dispatch for the same call
    /// site — duplicate/garbage code that miscompiled the method (boxed
    /// values came back as 0). Snapshotting + rollback here makes a bail
    /// fully transparent.
    pub(super) fn try_emit_inline(&mut self, pc: usize) -> bool {
        let Some(site) = self.inline_sites.get(&pc).cloned() else {
            return false;
        };
        self.try_emit_inline_site(pc, &site)
    }

    /// [`Self::try_emit_inline`] for a body that is NOT the `inline_sites`
    /// entry for `pc`.
    ///
    /// A bimorphic site splices two different callee bodies behind two guards
    /// at one caller pc — two overriding subclasses are two different methods,
    /// which is the entire point of a two-way split — so exactly one of them
    /// can be the `inline_sites` entry. Both go through this function, so both
    /// get the same rollback set and the same deopt-metadata postcondition.
    pub(super) fn try_emit_inline_site(&mut self, pc: usize, site: &crate::InlineSite) -> bool {
        let buf_checkpoint = self.buf.pos();
        let stack_checkpoint = self.stack.clone();
        let oop_marks_checkpoint = self.stack_oop_marks.clone();
        let spill_checkpoint = self.next_spill_offset;
        // groovyjarjarasm-asm-handler-getexceptiontablesize-sigsegv-20260713:
        // the buffer/stack/oop-marks/spill rollback above is NOT the full set
        // of speculative side effects `try_emit_inline_body` can produce. Any
        // bytecode instruction it simulates (e.g. an inlined `invoke*` via
        // `emit_post_invoke_exception_check`) can also push a **patch-site
        // offset** -- a raw `usize` into `self.buf` -- onto one of these
        // deferred patch-list fields. Those offsets are only meaningful while
        // they point at the placeholder bytes (`0F 84 00 00 00 00` etc.) that
        // were live when they were recorded. A bail rewinds `self.buf` past
        // them (via `rewind_to` above) and the fall-through normal-call path
        // then emits *different* code over that same buffer range -- but
        // without this snapshot/truncate, the stale offset(s) from the
        // abandoned attempt survive in the Vec and get blindly patched later
        // (`emit_exception_check_stub` / `emit_deopt_stubs` / `patch_branches`
        // / `patch_self_calls`, all of which run once at the very end of
        // `compile_bytecode` over the FINAL, already-reused buffer), corrupting
        // whatever real instruction now lives at that stale offset. Root-caused
        // via a live trace on `groovyjarjarasm.asm.Handler.getExceptionTableSize`
        // (pulled in by Groovy's ASM-based class generation under
        // `GroovyScriptFactoryTests`): a stale `exception_check_stubs` entry
        // from a rewound inline attempt got patched into the middle of the
        // *kept* method's precise-maps safepoint-id store, scribbling a bogus
        // immediate byte and a corrupt REX prefix into otherwise-valid JIT
        // code -- an immediate SIGSEGV the instant the (very hot, called
        // thousands of times) method next ran, well before any test
        // discovery. Snapshot every such deferred patch-list field here and
        // truncate back on bail, mirroring the buffer/stack rollback above.
        let exception_check_stubs_checkpoint = self.exception_check_stubs.len();
        let deopt_stubs_checkpoint = self.deopt_stubs.len();
        let forward_patches_checkpoint = self.forward_patches.len();
        let jump_table_patches_checkpoint = self.jump_table_patches.len();
        let self_call_patches_checkpoint = self.self_call_patches.len();
        let bounds_check_stubs_checkpoint = self.bounds_check_stubs.len();
        let null_check_store_stubs_checkpoint = self.null_check_store_stubs.len();
        // Reload-elision mirror: the inline mini-emitter replays CALLEE
        // bytecode whose internal joins the position rule cannot see (the
        // main loop's branch-target invalidation covers only OUTER-method
        // pcs). Suppress the mechanism for the duration and drop any live
        // mirror on both entry and exit; a bail additionally rewinds the
        // buffer, which would otherwise let a stale recorded position
        // "validate" against different, re-emitted code.
        let mirror_suppressed_checkpoint = self.slot_mirror_suppressed;
        self.slot_mirror = None;
        self.slot_mirror_suppressed = true;
        let deopt_points_checkpoint = self.deopt_points.len();
        let inline_ok = self.try_emit_inline_body(pc, site);
        self.slot_mirror_suppressed = mirror_suppressed_checkpoint;
        self.slot_mirror = None;
        // PGO-02 §3, enforced rather than argued.
        //
        // Every inlined body — statically bound or behind a receiver guard —
        // is entered and left inside ONE frame, the caller's own, and deopt
        // metadata has no way to say otherwise: `deopt::FrameState::caller`
        // exists but no producer populates it, so an inlined scope is not
        // representable. A deopt point published from inside a spliced body
        // would therefore name the CALLER's method with the CALLEE's bci — a
        // well-formed description of a stack that never existed, which is the
        // exact failure class the 2026-08-01 deopt-metadata audit found three
        // of.
        //
        // The safety argument used to be a claim about the source ("the
        // emitter contains no `build_and_record_deopt_point` on this path").
        // That claim is one future edit away from being false, and nothing
        // would fail when it became false. Check the postcondition instead: if
        // the body published any deopt metadata, refuse the splice and take
        // the real call. A refusal costs one dispatch; the alternative costs a
        // wrong stack.
        let published_deopt_metadata = self.deopt_stubs.len() > deopt_stubs_checkpoint
            || self.deopt_points.len() > deopt_points_checkpoint;
        if inline_ok && !published_deopt_metadata {
            true
        } else {
            // A body that published deopt metadata is rolled back through the
            // SAME path a mid-body bail takes — including the deopt lists
            // themselves, which the truncations below cover.
            // Discard every speculative side effect of the abandoned
            // inline attempt so the fall-through normal-call path starts
            // from exactly the pre-inline machine state.
            self.buf.rewind_to(buf_checkpoint);
            self.stack = stack_checkpoint;
            self.stack_oop_marks = oop_marks_checkpoint;
            self.next_spill_offset = spill_checkpoint;
            self.exception_check_stubs
                .truncate(exception_check_stubs_checkpoint);
            self.deopt_stubs.truncate(deopt_stubs_checkpoint);
            self.forward_patches.truncate(forward_patches_checkpoint);
            self.jump_table_patches
                .truncate(jump_table_patches_checkpoint);
            self.self_call_patches
                .truncate(self_call_patches_checkpoint);
            self.bounds_check_stubs
                .truncate(bounds_check_stubs_checkpoint);
            self.null_check_store_stubs
                .truncate(null_check_store_stubs_checkpoint);
            self.deopt_points.truncate(deopt_points_checkpoint);
            false
        }
    }

    /// Test-only injection point for the deopt-metadata postcondition above.
    ///
    /// A guard nobody can make fire is a guard nobody has tested. This lets
    /// `inline_publishing_a_deopt_point_is_refused` produce the one state the
    /// check exists to catch — a spliced body that published deopt metadata —
    /// without waiting for a future emitter change to produce it accidentally.
    #[cfg(test)]
    pub(super) fn force_inline_deopt_publication(&mut self) {
        self.deopt_stubs.push((self.buf.pos(), 0, 0));
    }

    /// Inline-emission body. MUST only be called via [`Self::try_emit_inline`],
    /// which snapshots and restores compiler state around it. A `false`
    /// return from anywhere inside is safe precisely because of that
    /// wrapper — the bail sites here therefore no longer need to unwind
    /// `next_spill_offset` by hand.
    fn try_emit_inline_body(&mut self, pc: usize, site: &crate::InlineSite) -> bool {
        let site = site.clone();
        #[cfg(test)]
        if super::INLINE_TEST_PUBLISHES_DEOPT.with(std::cell::Cell::get) {
            self.force_inline_deopt_publication();
        }

        // An inlined callee emits arbitrary code that uses the caller-saved
        // scratch GPRs (R8/R9) and FP temporaries (XMM0-7) — exactly the
        // registers the deferred-spill operand model (`StackSlot::Scratch` /
        // `StackSlot::Xmm`) parks live values in. Inlining is a call boundary,
        // so flush every caller-live Scratch/Xmm operand to its frame slot
        // BEFORE emitting the callee body — otherwise the callee clobbers a
        // value still live on the caller's operand stack (e.g. a computed
        // double argument or a result held across the call), silently
        // corrupting it. Without this, `leaf(x) + leaf(x*0.5)` and the whole
        // commons-math FastMath.sin family miscompiled under JIT. `try_emit_inline`
        // snapshots+restores all state, so a later mid-body bail rolls this back.
        self.flush_scratch_registers();

        let callee_code = &site.callee_code;
        let callee_len = site.callee_code_len;
        let callee_num_args = site.callee_num_args;
        let callee_max_locals = site.callee_max_locals;
        let _return_type = site.return_type;
        let (callee_param_jvm_slots, callee_param_slot_span) =
            crate::compute_param_jvm_slots(&site.descriptor, site.callee_is_static);
        if callee_param_jvm_slots.len() != callee_num_args {
            return false;
        }

        let callee_locals_size = callee_max_locals.max(callee_param_slot_span);
        // Allocate callee locals in caller's spill area.
        let Some(callee_local_base) = self.reserve_spill_slots(callee_locals_size) else {
            return false;
        };

        // Pop arguments from caller stack and store into callee locals.
        // Args are pushed left-to-right, so stack top = last arg.
        // For instance methods, arg0 = objectref ('this').
        let stack_len = self.stack.len();
        if stack_len < callee_num_args {
            self.next_spill_offset = callee_local_base;
            return false;
        }

        // Store args into the callee's JVM local slots. Category-2 parameters
        // consume two JVM slots while the JIT operand stack carries one i64
        // value, so the descriptor-derived slot map must mirror the normal
        // prologue layout (`(JJI)J` -> slots 0, 2, 4).
        for i in (0..callee_num_args).rev() {
            let slot = self.pop_stack();
            let local_idx = callee_param_jvm_slots[i];
            let local_off = callee_local_base + (local_idx as i32) * 8; // Cast: x86-64 immediate encoding
            self.load_slot_to_reg(RAX, slot);
            self.emit_store_local(local_off, RAX);
        }

        // Zero-init every non-parameter callee local. Do not start at
        // `callee_num_args`: that compact arg count is not a JVM local index
        // when category-2 parameters are present.
        for i in 0..callee_locals_size {
            if callee_param_jvm_slots.contains(&i) {
                continue;
            }
            let local_off = callee_local_base + (i as i32) * 8; // Cast: x86-64 immediate encoding
            self.emit_xor_reg_self(RAX);
            self.emit_store_local(local_off, RAX);
        }

        // Track forward branches within the inlined code: (patch_offset, target_callee_pc)
        let mut branch_patches: Vec<(usize, usize)> = Vec::new();
        // Map callee PC → native offset for branch targets
        let mut callee_pc_to_native: Vec<i64> = vec![-1; callee_len + 1];

        let mut cpc: usize = 0;
        let save_spill = self.next_spill_offset;
        // Operand-stack depth belonging to the CALLER; the callee's operands
        // sit above it. The callee operand stack is "empty" exactly when
        // `self.stack.len() == caller_base_depth`.
        let caller_base_depth = self.stack.len();
        // Callee branch targets (merge points). Branchy callees inline only
        // when every merge has an EMPTY callee operand stack (enforced by the
        // merge-point reset below + the per-branch checks). 419a6f5 blanket-
        // bailed ALL branches to stop a value-merge slot desync (the
        // `iconst_1; goto L; iconst_0; L: ireturn` diamond); this restores the
        // provably-safe subset (e.g. `x>=0?x:-x`) while still bailing diamonds.
        let callee_branch_targets = compute_branch_targets(callee_code, callee_len);
        // True after an instruction that does NOT fall through (goto/return/
        // athrow) so the merge-point check can distinguish a dead fall-through
        // (stale slots — safe to reset) from a live value-merge (must bail).
        let mut prev_was_terminator = false;

        while cpc < callee_len {
            callee_pc_to_native[cpc] = self.buf.pos() as i64; // Cast: address arithmetic
            let op = callee_code[cpc];

            // Merge-point handling: at a branch target the callee operand
            // stack must be the canonical empty state (caller_base_depth). A
            // live path arriving with a value is a value-producing merge the
            // spill-slot model can't represent soundly -> bail. A dead fall-
            // through (previous instr was a terminator) only left stale slots;
            // reset them so the target starts from the empty state every
            // branch into it also guarantees (branches require empty stack).
            if callee_branch_targets.get(cpc).copied().unwrap_or(false) {
                if !prev_was_terminator && self.stack.len() != caller_base_depth {
                    self.next_spill_offset = callee_local_base;
                    return false;
                }
                self.stack.truncate(caller_base_depth);
                self.stack_oop_marks.truncate(caller_base_depth);
                self.next_spill_offset = save_spill;
            }

            match op {
                // nop
                0x00 => {
                    cpc += 1;
                }

                // aconst_null
                0x01 => {
                    self.emit_xor_reg_self(RAX);
                    self.push_from_rax();
                    cpc += 1;
                }

                // iconst_m1..iconst_5
                0x02..=0x08 => {
                    let val = (op as i32) - 3; // Widening: always safe
                    self.emit_mov_imm32_sx(RAX, val);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lconst_0, lconst_1
                0x09 | 0x0a => {
                    let val = (op as i64) - 9; // Widening: always safe
                    self.emit_mov_imm32_sx(RAX, val as i32); // Cast: x86-64 immediate encoding
                    self.push_from_rax();
                    cpc += 1;
                }

                // fconst_0, fconst_1, fconst_2
                0x0b..=0x0d => {
                    let fval: f32 = (op - 0x0b) as f32; // Cast: JIT ABI convention
                    let bits = fval.to_bits() as i64; // Cast: JIT ABI convention
                    self.emit_mov_imm32_sx(RAX, bits as i32); // Cast: x86-64 immediate encoding
                    self.push_from_rax();
                    cpc += 1;
                }

                // dconst_0, dconst_1
                0x0e | 0x0f => {
                    let dval: f64 = (op - 0x0e) as f64; // Cast: JIT ABI convention
                    let bits = dval.to_bits() as i64; // Cast: JIT ABI convention
                    if bits == 0 {
                        self.emit_xor_reg_self(RAX);
                    } else {
                        self.emit_mov_imm64(RAX, bits);
                    }
                    self.push_from_rax();
                    cpc += 1;
                }

                // bipush
                0x10 => {
                    if cpc + 1 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let val = callee_code[cpc + 1] as i8 as i32; // Widening: always safe
                    self.emit_mov_imm32_sx(RAX, val);
                    self.push_from_rax();
                    cpc += 2;
                }

                // sipush
                0x11 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let val =
                        i16::from_be_bytes([callee_code[cpc + 1], callee_code[cpc + 2]]) as i32; // Widening: always safe
                    self.emit_mov_imm32_sx(RAX, val);
                    self.push_from_rax();
                    cpc += 3;
                }

                // ldc
                //
                // `site.ldc_info` / `site.ldc2w_info` are keyed by the
                // callee bytecode PC (see the inline resolver in
                // `try_jit_compile_callee`), NOT the CP index. Match on
                // `cpc` — the prior CP-index lookup silently missed and
                // pushed 0 for the constant.
                //
                // A MISS IS A REFUSAL, NEVER A ZERO. Those tables carry only
                // the constants this emitter can model as an immediate —
                // `Integer` and `Float`. Every other `ldc` kind (String, Class,
                // MethodHandle, MethodType, condy) names a constant whose value
                // is a *reference*, materialised at run time by
                // `helpers.ldc_string` / `helpers.ldc_class_cp`, which this
                // mini-emitter does not call. Substituting 0 for one is wrong
                // code: it pushes `null` where the callee's body pushes a live
                // object. That is not theoretical — it shipped. A one-line
                // `Dialect.extractPattern(unit) { return "extract(?1 from ?2)"; }`
                // inlined into `H2Dialect.extractPattern` compiled to
                // `xor eax,eax; ret`, so every Hibernate HQL `extract()` /
                // `cast()` / `str()` query died in
                // `PatternRenderer.<init>` with
                // `NullPointerException: ... because "pattern" is null` — 17 of
                // the 34 method failures across FunctionTests /
                // StandardFunctionTests / ASTParserLoadingTest / HQLTest on the
                // 2026-08-11 Linux full-suite run, all green under `--nojit`.
                // Refusing costs one real call; this cost correct answers.
                0x12 => {
                    if cpc + 1 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let Some((_, val)) = site.ldc_info.iter().find(|(p, _)| *p == cpc) else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    };
                    self.emit_mov_imm32_sx(RAX, *val as i32); // Cast: x86-64 immediate encoding
                    self.push_from_rax();
                    cpc += 2;
                }

                // ldc_w — same contract as `ldc` above.
                0x13 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let Some((_, val)) = site.ldc_info.iter().find(|(p, _)| *p == cpc) else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    };
                    self.emit_mov_imm32_sx(RAX, *val as i32); // Cast: x86-64 immediate encoding
                    self.push_from_rax();
                    cpc += 3;
                }

                // ldc2_w — same contract. Only `Long` and `Double` are
                // modellable here; anything else is a refusal, not a zero.
                0x14 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let Some((_, val)) = site.ldc2w_info.iter().find(|(p, _)| *p == cpc) else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    };
                    self.emit_mov_imm64(RAX, *val);
                    self.push_from_rax();
                    cpc += 3;
                }

                // iload, lload, fload, dload, aload
                0x15 | 0x16 | 0x17 | 0x18 | 0x19 => {
                    if cpc + 1 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let idx = callee_code[cpc + 1] as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 2;
                }

                // iload_0..iload_3
                0x1a..=0x1d => {
                    let idx = (op - 0x1a) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lload_0..lload_3
                0x1e..=0x21 => {
                    let idx = (op - 0x1e) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 1;
                }

                // fload_0..fload_3
                0x22..=0x25 => {
                    let idx = (op - 0x22) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 1;
                }

                // dload_0..dload_3
                0x26..=0x29 => {
                    let idx = (op - 0x26) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 1;
                }

                // aload_0..aload_3
                0x2a..=0x2d => {
                    let idx = (op - 0x2a) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 1;
                }

                // istore, lstore, fstore, dstore, astore
                0x36 | 0x37 | 0x38 | 0x39 | 0x3a => {
                    if cpc + 1 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let idx = callee_code[cpc + 1] as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 2;
                }

                // istore_0..istore_3
                0x3b..=0x3e => {
                    let idx = (op - 0x3b) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 1;
                }

                // lstore_0..lstore_3
                0x3f..=0x42 => {
                    let idx = (op - 0x3f) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 1;
                }

                // fstore_0..fstore_3
                0x43..=0x46 => {
                    let idx = (op - 0x43) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 1;
                }

                // dstore_0..dstore_3
                0x47..=0x4a => {
                    let idx = (op - 0x47) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 1;
                }

                // astore_0..astore_3
                0x4b..=0x4e => {
                    let idx = (op - 0x4b) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 1;
                }

                // pop
                0x57 => {
                    let _ = self.pop_stack();
                    cpc += 1;
                }

                // pop2
                0x58 => {
                    let _ = self.pop_stack();
                    let _ = self.pop_stack();
                    cpc += 1;
                }

                // dup
                0x59 => {
                    let slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, slot);
                    self.push_from_rax();
                    self.push_from_rax();
                    cpc += 1;
                }

                // swap
                0x5f => {
                    let a = self.pop_stack();
                    let b = self.pop_stack();
                    // Read BOTH operands before the first push: push_from_rax
                    // reuses the just-reclaimed lower spill slot, which is
                    // exactly `b`'s frame slot when both operands are
                    // frame-resident — storing into it before reading `b`
                    // duplicated value1 into both result slots.
                    self.load_slot_to_reg(RAX, a);
                    self.load_slot_to_reg(RCX, b);
                    self.push_from_rax();
                    self.emit_mov_reg_reg(RAX, RCX);
                    self.push_from_rax();
                    cpc += 1;
                }

                // iadd
                0x60 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    // ADD RAX, RCX
                    self.rex_w();
                    self.buf.emit(&[0x01, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ladd
                0x61 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x01, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // isub
                0x64 => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    // SUB RAX, RCX
                    self.rex_w();
                    self.buf.emit(&[0x29, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lsub
                0x65 => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    self.rex_w();
                    self.buf.emit(&[0x29, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // imul
                0x68 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    // IMUL RAX, RCX
                    self.rex_w();
                    self.buf.emit(&[0x0F, 0xAF, 0xC1]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lmul
                0x69 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x0F, 0xAF, 0xC1]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // idiv — JVMS-compliant: guards divide-by-zero (→ deopt to
                // throw ArithmeticException) and INT_MIN / -1 (→ INT_MIN,
                // matches dividend) before issuing CDQ; IDIV ECX.
                0x6c => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    self.emit_safe_idiv(cpc, /*is_64bit*/ false, /*is_rem*/ false);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ldiv — JVMS-compliant guards; emits CQO; IDIV RCX with the
                // LONG_MIN / -1 overflow special-case materialised inline.
                0x6d => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    self.emit_safe_idiv(cpc, /*is_64bit*/ true, /*is_rem*/ false);
                    self.push_from_rax();
                    cpc += 1;
                }

                // irem — JVMS-compliant guards; INT_MIN % -1 yields 0.
                0x70 => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    self.emit_safe_idiv(cpc, /*is_64bit*/ false, /*is_rem*/ true);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lrem — JVMS-compliant guards; LONG_MIN % -1 yields 0.
                0x71 => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    self.emit_safe_idiv(cpc, /*is_64bit*/ true, /*is_rem*/ true);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ineg
                0x74 => {
                    self.pop_to_rax();
                    // NEG EAX
                    self.buf.emit(&[0xF7, 0xD8]);
                    // MOVSXD RAX, EAX
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lneg
                0x75 => {
                    self.pop_to_rax();
                    // NEG RAX
                    self.rex_w();
                    self.buf.emit(&[0xF7, 0xD8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ishl
                0x78 => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    // SHL EAX, CL
                    self.buf.emit(&[0xD3, 0xE0]);
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lshl
                0x79 => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    // SHL RAX, CL
                    self.rex_w();
                    self.buf.emit(&[0xD3, 0xE0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ishr
                0x7a => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    // SAR EAX, CL
                    self.buf.emit(&[0xD3, 0xF8]);
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lshr
                0x7b => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    self.rex_w();
                    self.buf.emit(&[0xD3, 0xF8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // iushr
                0x7c => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    // SHR EAX, CL
                    self.buf.emit(&[0xD3, 0xE8]);
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lushr
                0x7d => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    self.rex_w();
                    self.buf.emit(&[0xD3, 0xE8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // iand
                0x7e => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x21, 0xC8]); // AND RAX, RCX
                    self.push_from_rax();
                    cpc += 1;
                }

                // land
                0x7f => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x21, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ior
                0x80 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x09, 0xC8]); // OR RAX, RCX
                    self.push_from_rax();
                    cpc += 1;
                }

                // lor
                0x81 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x09, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ixor
                0x82 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x31, 0xC8]); // XOR RAX, RCX
                    self.push_from_rax();
                    cpc += 1;
                }

                // lxor
                0x83 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x31, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // iinc
                0x84 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let idx = callee_code[cpc + 1] as usize; // Widening: always safe
                    let inc = callee_code[cpc + 2] as i8 as i32; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    // ADD RAX, imm32
                    self.rex_w();
                    self.buf.emit_byte(0x05);
                    self.buf.emit(&inc.to_le_bytes());
                    self.emit_store_local(local_off, RAX);
                    cpc += 3;
                }

                // i2l — identity in our i64 representation
                0x85 => {
                    cpc += 1;
                }

                // l2i — truncate to 32-bit, sign-extend back
                0x88 => {
                    self.pop_to_rax();
                    // MOVSXD RAX, EAX
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // i2b
                0x91 => {
                    self.pop_to_rax();
                    // MOVSX RAX, AL
                    self.rex_w();
                    self.buf.emit(&[0x0F, 0xBE, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // i2c
                0x92 => {
                    self.pop_to_rax();
                    // MOVZX EAX, AX (zero-extend 16-bit)
                    self.buf.emit(&[0x0F, 0xB7, 0xC0]);
                    // Upper 32 bits auto-zeroed
                    self.push_from_rax();
                    cpc += 1;
                }

                // i2s
                0x93 => {
                    self.pop_to_rax();
                    // MOVSX EAX, AX (sign-extend 16-bit)
                    self.buf.emit(&[0x0F, 0xBF, 0xC0]);
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lcmp
                0x94 => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    // CMP RAX, RCX
                    self.rex_w();
                    self.buf.emit(&[0x39, 0xC8]);
                    // SETG AL (1 if >)
                    self.buf.emit(&[0x0F, 0x9F, 0xC0]);
                    // MOVZX EAX, AL
                    self.buf.emit(&[0x0F, 0xB6, 0xC0]);
                    // SETL CL
                    self.buf.emit(&[0x0F, 0x9C, 0xC1]);
                    // MOVZX ECX, CL
                    self.buf.emit(&[0x0F, 0xB6, 0xC9]);
                    // SUB EAX, ECX
                    self.buf.emit(&[0x29, 0xC8]);
                    // MOVSXD RAX, EAX
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ifeq..ifle (0x99..0x9e) — conditional branch on <cmp> 0.
                //
                // Restored (419a6f5 blanket-bailed all branches). Inlining a
                // branchy callee is sound in this single-linear-pass emitter
                // ONLY when no operand is live across a merge: operand-stack
                // slots are handed out by a growing `next_spill_offset`, so two
                // paths reaching a merge with a value would hold it in
                // different frame slots (the `iconst_1; goto L; iconst_0; L:
                // ireturn` diamond — the bug 419a6f5 fixed). We therefore
                // require the callee operand stack to be EMPTY after the branch
                // pops its operands (here) AND at every target (the merge-point
                // reset at the loop top); any value-merge bails. Forward
                // branches only — backward edges (loops) re-enter an already-
                // emitted target whose slot layout we can't re-canonicalise.
                0x99..=0x9e => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let offset =
                        i16::from_be_bytes([callee_code[cpc + 1], callee_code[cpc + 2]]) as i32; // Widening: always safe
                    let target = (cpc as i32 + offset) as usize; // Cast: x86-64 immediate encoding
                    if target <= cpc {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.pop_to_rax();
                    if self.stack.len() != caller_base_depth {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.buf.emit(&[0x85, 0xC0]); // TEST EAX, EAX
                    let cc = match op {
                        0x99 => 0x84u8, // JE
                        0x9a => 0x85,   // JNE
                        0x9b => 0x8C,   // JL
                        0x9c => 0x8D,   // JGE
                        0x9d => 0x8F,   // JG
                        0x9e => 0x8E,   // JLE
                        _ => unreachable!(),
                    };
                    self.buf.emit(&[0x0F, cc]);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, target));
                    cpc += 3;
                }

                // if_icmpeq..if_icmple (0x9f..0xa4) — int compare branch. Same
                // empty-stack-at-merge safety as 0x99..0x9e above.
                0x9f..=0xa4 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let offset =
                        i16::from_be_bytes([callee_code[cpc + 1], callee_code[cpc + 2]]) as i32; // Widening: always safe
                    let target = (cpc as i32 + offset) as usize; // Cast: x86-64 immediate encoding
                    if target <= cpc {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    if self.stack.len() != caller_base_depth {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.load_slot_to_reg(RCX, top);
                    self.buf.emit(&[0x39, 0xC8]); // CMP EAX, ECX
                    let cc = match op {
                        0x9f => 0x84u8, // JE
                        0xa0 => 0x85,   // JNE
                        0xa1 => 0x8C,   // JL
                        0xa2 => 0x8D,   // JGE
                        0xa3 => 0x8F,   // JG
                        0xa4 => 0x8E,   // JLE
                        _ => unreachable!(),
                    };
                    self.buf.emit(&[0x0F, cc]);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, target));
                    cpc += 3;
                }

                // goto (0xa7) — unconditional forward branch. Same safety.
                0xa7 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let offset =
                        i16::from_be_bytes([callee_code[cpc + 1], callee_code[cpc + 2]]) as i32; // Widening: always safe
                    let target = (cpc as i32 + offset) as usize; // Cast: x86-64 immediate encoding
                    if target <= cpc {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    if self.stack.len() != caller_base_depth {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.buf.emit_byte(0xE9); // JMP rel32
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, target));
                    cpc += 3;
                }

                // ireturn, lreturn, areturn, freturn, dreturn
                0xac | 0xad | 0xb0 | 0xae | 0xaf => {
                    // `areturn` returns an object reference, and the popped
                    // entry's own mark says the same thing. The pop/push pair
                    // below moves the value to a new slot, and `push_from_rax`
                    // always marks its push `false` — so without carrying the
                    // mark across, inlining a reference-returning callee erases
                    // the oop tag of its result. Under moving-young that entry
                    // is then neither published nor rewritable.
                    let ret_is_oop =
                        op == 0xb0 || self.stack_oop_marks.last().copied().unwrap_or(false);
                    // Pop callee's return value → push onto caller stack
                    self.pop_to_rax();
                    // Reclaim callee locals
                    self.next_spill_offset = save_spill;
                    self.push_from_rax();
                    if ret_is_oop {
                        self.mark_top_as_oop();
                    }
                    // Jump past the rest of the inlined code
                    self.buf.emit_byte(0xE9);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    // Use callee_len as the "after inline" target
                    branch_patches.push((patch_off, callee_len));
                    cpc += 1;
                }

                // return (void)
                0xb1 => {
                    self.next_spill_offset = save_spill;
                    // Jump past the rest of the inlined code
                    self.buf.emit_byte(0xE9);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, callee_len));
                    cpc += 1;
                }

                // getfield (0xb4) — use callee's field_info
                0xb4 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.flush_scratch_registers();
                    // `site.field_info` is keyed by the callee bytecode PC
                    // (`fpc` in `try_jit_compile_callee` / the inline
                    // resolver), NOT by the constant-pool index. Using the
                    // CP index here silently mismatched — a multi-field
                    // callee could pick another field op's `field_index`
                    // and read/write the wrong slot. Look up by `cpc`.
                    if let Some((_, field_index, type_tag)) =
                        site.field_info.iter().find(|(p, _, _)| *p == cpc).copied()
                    {
                        let obj_slot = self.pop_stack();
                        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                        self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                        self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                        crate::metrics::note_getfield_arm(0);
                        self.emit_call_absolute(self.helpers.getfield);
                        // The checked helper returns the `i64::MIN` deopt/NPE
                        // sentinel on a bad (stale/corrupt) receiver instead of a
                        // real field value — without this guard the sentinel is
                        // pushed and treated as legitimate data, turning what used
                        // to be an immediate SIGSEGV into silent corruption/hangs
                        // downstream. Mirrors the invoke-site guard below.
                        self.emit_post_invoke_exception_check(type_tag);
                        self.push_from_rax();
                        // Mirrors the top-level `getfield` arms and the inlined
                        // `getstatic` arm just below: a reference field's value
                        // is a live oop and must be tagged, or it is invisible
                        // to both the precise oop map and the shadow stack.
                        if type_tag == b'L' || type_tag == b'[' {
                            self.mark_top_as_oop();
                        }
                    } else {
                        // Cannot resolve field — bail out
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    cpc += 3;
                }

                // putfield (0xb5) — use callee's field_info
                0xb5 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.flush_scratch_registers();
                    // Keyed by callee bytecode PC — see the `getfield`
                    // note above. Mismatching CP index vs PC here is the
                    // bug that dropped constructor field writes (boxed
                    // ints / `String.value` came back as 0).
                    if let Some((_, field_index, type_tag)) =
                        site.field_info.iter().find(|(p, _, _)| *p == cpc).copied()
                    {
                        let val_slot = self.pop_stack();
                        let obj_slot = self.pop_stack();
                        // Make a null receiver a real Java NPE before either
                        // the inline store or a legacy helper can turn it into
                        // a silent no-op. Protected sites retain their precise
                        // exceptional frame for javac monitor cleanup.
                        self.load_slot_to_reg(RAX, obj_slot);
                        self.emit_precise_null_check_field_store();
                        if type_tag == b'L' || type_tag == b'[' {
                            let compact_offset = site
                                .compact_field_info
                                .iter()
                                .find(|(p, _, is_ref)| *p == cpc && *is_ref)
                                .map(|(_, offset, _)| *offset);
                            let fresh_ctor_first_store =
                                inline_site_is_fresh_ctor_first_store(&site, cpc, field_index);
                            if inline_putfield_enabled()
                                && !narrow_oops_block_inline_fields()
                                && cratonvm_types::compact_ref_fields_enabled()
                                && self.helpers.region_bounds_addr != 0
                            {
                                if let Some(offset) = compact_offset {
                                    if fresh_ctor_first_store {
                                        self.emit_inline_fresh_ctor_compact_ref_putfield(
                                            obj_slot,
                                            val_slot,
                                            field_index,
                                            offset,
                                        );
                                    } else {
                                        self.emit_inline_body_compact_ref_putfield(
                                            obj_slot,
                                            val_slot,
                                            field_index,
                                            offset,
                                        );
                                    }
                                } else {
                                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                                    self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                                    self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32);
                                    self.load_slot_to_reg(ARG_REGS[3], val_slot);
                                    self.emit_call_absolute(self.helpers.putfield_object);
                                }
                            } else {
                                self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                                self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                                self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                                self.load_slot_to_reg(ARG_REGS[3], val_slot);
                                self.emit_call_absolute(self.helpers.putfield_object);
                            }
                        } else {
                            self.load_slot_to_reg(ARG_REGS[0], obj_slot);
                            self.emit_mov_imm32_sx(ARG_REGS[1], field_index as i32); // Cast: x86-64 immediate encoding
                            self.load_slot_to_reg(ARG_REGS[2], val_slot);
                            let helper = match type_tag {
                                b'J' => self.helpers.putfield_long,
                                b'F' => self.helpers.putfield_float,
                                b'D' => self.helpers.putfield_double,
                                _ => self.helpers.putfield_int,
                            };
                            self.emit_call_absolute(helper);
                        }
                    } else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    cpc += 3;
                }

                // getstatic (0xb2) — use callee's static_field_info
                //
                // Same direct-load-or-helper split as the top-level 0xb2 arm;
                // the long note there explains what is baked and which sites
                // still take `jit_getstatic`.
                0xb2 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    // `static_field_info` is keyed by callee bytecode PC,
                    // not CP index — match on `cpc` (see the `getfield`
                    // note above).
                    if let Some((_, class_id_raw, field_index, type_tag, is_volatile)) = site
                        .static_field_info
                        .iter()
                        .find(|(p, _, _, _, _)| *p == cpc)
                        .copied()
                    {
                        // Direct load, no helper CALL — see the top-level 0xb2
                        // arm. `flush_scratch_registers` deliberately moved
                        // INSIDE the helper branch: the inline form clobbers
                        // only RAX, so spilling the operand-stack cache for it
                        // would give back part of what it saves. The `else`
                        // arm below abandons the whole inline attempt, so it
                        // needs no flush either.
                        if !self.try_emit_inline_getstatic(
                            class_id_raw,
                            field_index,
                            type_tag,
                            is_volatile,
                        ) {
                            self.flush_scratch_registers();
                            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                            self.emit_mov_imm32_sx(ARG_REGS[1], class_id_raw as i32); // Cast: x86-64 immediate encoding
                            self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                            self.emit_call_absolute(self.helpers.getstatic);
                            // jit-linewrapper-flushtype-npe fix (2026-07-17):
                            // see the matching fix + comment at the top-level
                            // 0xb2 arm -- same helper, same missing
                            // post-invoke exception check for a `<clinit>`
                            // failure surfaced via the deopt sentinel.
                            self.emit_post_invoke_exception_check(type_tag);
                            // Volatile static: emit MFENCE after read (SeqCst acquire)
                            if is_volatile {
                                self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
                            }
                            self.push_from_rax();
                            // T1.1.a (fix, 2026-07-07) — see the matching fix at
                            // the top-level 0xb2 arm: a reference-typed static
                            // field must not keep push_from_rax's default
                            // non-oop mark, or it decodes wrong in a precise
                            // GC/deopt oop map while live.
                            if type_tag == b'L' || type_tag == b'[' {
                                self.mark_top_as_oop();
                            }
                        }
                    } else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    cpc += 3;
                }

                // putstatic (0xb3) — use callee's static_field_info
                //
                // Helper-only, like the top-level 0xb3 arm: the address
                // machinery exists now, but the SATB pre-barrier and the
                // first-touch block creation live in `set_static_shared`. See
                // the note on the top-level 0xb3 arm.
                0xb3 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.flush_scratch_registers();
                    // Keyed by callee bytecode PC — match on `cpc`.
                    if let Some((_, class_id_raw, field_index, type_tag, is_volatile)) = site
                        .static_field_info
                        .iter()
                        .find(|(p, _, _, _, _)| *p == cpc)
                        .copied()
                    {
                        let val_slot = self.pop_stack();
                        let helper_fn: usize = match type_tag {
                            b'J' => self.helpers.putstatic_long,
                            b'F' => self.helpers.putstatic_float,
                            b'D' => self.helpers.putstatic_double,
                            b'L' | b'[' => self.helpers.putstatic_object,
                            _ => self.helpers.putstatic_int,
                        };
                        // Volatile static: emit MFENCE before write (SeqCst release)
                        if is_volatile {
                            self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
                        }
                        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                        self.emit_mov_imm32_sx(ARG_REGS[1], class_id_raw as i32); // Cast: x86-64 immediate encoding
                        self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                        self.load_slot_to_reg(ARG_REGS[3], val_slot);
                        self.emit_call_absolute(helper_fn);
                        // jit-putstatic-clinit-gap fix (2026-07-17): the
                        // helper now runs `<clinit>` on first touch before
                        // writing and, on failure, returns the `i64::MIN`
                        // deopt sentinel instead of `0` (mirrors
                        // `jit_getstatic`'s sentinel; see the fix comment on
                        // `jit_putstatic_class_init_guard` in
                        // `vm/src/jit/helpers.rs`). `putstatic` is
                        // void-returning, so route it through the shared
                        // void-helper exception-check convention (same one
                        // the `invokestatic` arraycopy dispatch call uses)
                        // rather than pushing a value.
                        self.emit_post_invoke_exception_check(b'V');
                        // Volatile static: emit MFENCE after write (SeqCst store-load barrier)
                        if is_volatile {
                            self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
                        }
                    } else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    cpc += 3;
                }

                // if_acmpeq/if_acmpne (0xa5/0xa6) — reference compare branch.
                // Same empty-stack-at-merge safety as 0x99..0x9e.
                0xa5 | 0xa6 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let offset =
                        i16::from_be_bytes([callee_code[cpc + 1], callee_code[cpc + 2]]) as i32; // Widening: always safe
                    let target = (cpc as i32 + offset) as usize; // Cast: x86-64 immediate encoding
                    if target <= cpc {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    if self.stack.len() != caller_base_depth {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.load_slot_to_reg(RCX, top);
                    self.rex_w();
                    self.buf.emit(&[0x39, 0xC8]); // CMP RAX, RCX
                    let cc = if op == 0xa5 { 0x84u8 } else { 0x85 }; // JE / JNE
                    self.buf.emit(&[0x0F, cc]);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, target));
                    cpc += 3;
                }

                // ifnull/ifnonnull (0xc6/0xc7) — null-compare branch. Same safety.
                0xc6 | 0xc7 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let offset =
                        i16::from_be_bytes([callee_code[cpc + 1], callee_code[cpc + 2]]) as i32; // Widening: always safe
                    let target = (cpc as i32 + offset) as usize; // Cast: x86-64 immediate encoding
                    if target <= cpc {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.pop_to_rax();
                    if self.stack.len() != caller_base_depth {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.rex_w();
                    self.buf.emit(&[0x85, 0xC0]); // TEST RAX, RAX
                    let cc = if op == 0xc6 { 0x84u8 } else { 0x85 }; // JE / JNE
                    self.buf.emit(&[0x0F, cc]);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, target));
                    cpc += 3;
                }

                // invokespecial (0xb7) — ONLY resolver-proven no-op super
                // constructor calls (`site.elided_invoke_pcs`): the target is
                // `java/lang/Object.<init>()V` (or an elidable trivial chain
                // to it), so the call has no observable effect. Pop the
                // receiver the preceding `aload_0` pushed — a compile-time
                // stack-model adjustment, no machine code — and continue.
                // This is what admits CONSTRUCTOR bodies to inlining. Any
                // other invokespecial bails to the dispatch fallback.
                0xb7 => {
                    if cpc + 2 >= callee_len || !site.elided_invoke_pcs.contains(&cpc) {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let _ = self.pop_stack();
                    cpc += 3;
                }

                // Unsupported opcode in inline context — bail out
                _ => {
                    // Restore spill offset and return false to fall back to a call
                    self.next_spill_offset = callee_local_base;
                    return false;
                }
            }

            // For the next iteration's merge-point check: goto (0xa7) and the
            // returns (0xac..=0xb1) jump away, so the following PC is reachable
            // only as a branch target (a dead fall-through whose stale slots
            // the merge-point reset may clear). Everything else falls through.
            prev_was_terminator = matches!(op, 0xa7 | 0xac..=0xb1);
        }

        // Mark the "after inline" position for return-jumps
        callee_pc_to_native[callee_len] = self.buf.pos() as i64; // Cast: address arithmetic

        // Patch all forward branches
        for (patch_off, target_cpc) in &branch_patches {
            let target_native = if *target_cpc < callee_pc_to_native.len() {
                callee_pc_to_native[*target_cpc]
            } else {
                self.buf.pos() as i64 // Cast: address arithmetic
            };
            if target_native < 0 {
                // Target not yet emitted (shouldn't happen for forward branches after full emission)
                // Fall back: point to current position
                let rel32 = (self.buf.pos() as i32) - (*patch_off as i32 + 4); // Cast: x86-64 rel32 displacement
                self.buf.try_patch_i32(*patch_off, rel32).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
            } else {
                let rel32 = (target_native as i32) - (*patch_off as i32 + 4); // Cast: x86-64 rel32 displacement
                self.buf.try_patch_i32(*patch_off, rel32).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
            }
        }

        // Reclaim callee local spill slots. The `ireturn` handler pushed any
        // return value AT `save_spill` (it set next_spill=save_spill then
        // push_from_rax, leaving next_spill=save_spill+8). Resetting next_spill
        // back to `save_spill` here would FREE that return-value slot, so the
        // next push (e.g. a sibling call's argument) reused it and clobbered the
        // value — `leaf(a) + leafBig(a)` miscompiled because `leaf(a)`'s result
        // was overwritten by `iload a` for leafBig's argument. Keep next_spill
        // above the live operand-stack top so the return value is preserved.
        let next_spill = if self.stack.len() > caller_base_depth {
            // A return value occupies one slot at `save_spill`.
            let Some(end) = self.checked_spill_range_end(save_spill, 1) else {
                return false;
            };
            end
        } else {
            // Void callee: nothing pushed, callee operand stack fully reclaimed.
            save_spill
        };
        self.next_spill_offset = next_spill;

        true
    }
}
