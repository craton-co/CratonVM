// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Array element loads and stores, and `arraylength` in the single-pass backend's bytecode walk.
//!
//! Split out of `Compiler::compile_bytecode` (`bytecode_walk.rs`), which keeps
//! the walk loop and one dispatch `match` that routes each opcode to its
//! family (`jit-god-functions-and-request-side-channels-FIXED-20260912.md`).
//! The arms are the walk's own, moved unchanged except for how they leave the
//! walk: `continue` became `return WalkStep::Next(pc)` and `return x` became
//! `return WalkStep::Return(x)`.

use super::bytecode_walk::*;
use super::*;

impl Compiler {
    /// Lower one bytecode of this family at `pc`. The result is the pc the walk
    /// continues at, or the value `compile_bytecode` returns.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn walk_array(
        &mut self,
        code: &[u8],
        code_len: usize,
        op: u8,
        mut pc: usize,
        _dead: &mut bool,
        branch_targets: &[bool],
        // `insn_starts` is the whole method's instruction-start map, built
        // once by `compile_bytecode` — see the comment at its definition for
        // why it is a parameter and not a cache, and why it is sized from
        // `code.len()` rather than from `code_len`.
        insn_starts: &[bool],
    ) -> WalkStep {
        match op {
            // iaload — load int from int[] array (inline)
            0x2e => {
                if self.try_array_load_through_homes(code, op, pc, insn_starts) {
                    // The element is held sign-extended: an `i2l` next is a no-op.
                    let next = self.skip_redundant_i2l(code, code_len, pc + 1, branch_targets);
                    return WalkStep::Next(next);
                }
                let index_slot = self.pop_stack();
                let array_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-9 HIGH fix: NPE on null array (JVMS §iaload).
                self.emit_null_check_array_load_at(code, pc, insn_starts);
                self.emit_bounds_check(pc);
                self.emit_int_aload_regs();
                self.push_from_rax();
                pc = self.skip_redundant_i2l(code, code_len, pc + 1, branch_targets);
            }

            // aaload — load reference from Object[] array (inline, 8 bytes/element)
            0x32 => {
                // zt t19/zb2: under an armed ZGC load barrier the element is
                // read by `jit_aaload`, which reads it through
                // `VmHeap::load_ref_slot_barriered` (row 29 of the t18/zb1
                // read inventory). See `walk_aaload_through_barrier_helper`.
                if zgc_read_barrier_blocks_inline_fields() {
                    // zt t21/zb4 (stage 2d): colored mode decodes inline.
                    if let Some(base) = zgc_colored_inline_load_barrier_base() {
                        // zt t22/zb5 (stage 2e): under the color flip, the
                        // bad-mask test and a `jit_aaload` slow branch.
                        if let Some(cell) = zgc_colored_flip_bad_mask_cell() {
                            return self.walk_aaload_colored_flip(
                                code,
                                pc,
                                insn_starts,
                                base,
                                cell,
                            );
                        }
                        return self.walk_aaload_colored_inline(code, pc, insn_starts, base);
                    }
                    return self.walk_aaload_through_barrier_helper(code, pc, insn_starts);
                }
                let index_slot = self.pop_stack();
                let array_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-9 HIGH fix: NPE on null array (JVMS §aaload).
                self.emit_null_check_array_load_at(code, pc, insn_starts);
                self.emit_bounds_check(pc);
                self.emit_ref_aload_regs();
                self.push_from_rax();
                // T1.1.a — aaload reads a reference from an Object[].
                self.mark_top_as_oop();
                pc += 1;
            }

            // laload — load long from long[] array (inline, 8 bytes/element)
            0x2f => {
                if self.try_array_load_through_homes(code, op, pc, insn_starts) {
                    return WalkStep::Next(pc + 1);
                }
                let index_slot = self.pop_stack();
                let array_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-9 HIGH fix: NPE on null array (JVMS §laload).
                self.emit_null_check_array_load_at(code, pc, insn_starts);
                self.emit_bounds_check(pc);
                self.emit_long_aload_regs();
                self.push_from_rax();
                pc += 1;
            }

            // faload — load float from float[] array (inline, 4 bytes/element)
            0x30 => {
                let index_slot = self.pop_stack();
                let array_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-9 HIGH fix: NPE on null array (JVMS §faload).
                self.emit_null_check_array_load_at(code, pc, insn_starts);
                self.emit_bounds_check(pc);
                // Float is 4 bytes, same as int; value is stored as bit pattern
                self.emit_int_aload_regs();
                self.push_from_rax_as_xmm0();
                pc += 1;
            }

            // daload — load double from double[] array (inline, 8 bytes/element)
            0x31 => {
                let index_slot = self.pop_stack();
                let array_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-9 HIGH fix: NPE on null array (JVMS §daload).
                self.emit_null_check_array_load_at(code, pc, insn_starts);
                self.emit_bounds_check(pc);
                // Double is 8 bytes, same as long; value is stored as bit pattern
                self.emit_long_aload_regs();
                self.push_from_rax_as_xmm0();
                pc += 1;
            }

            // baload — load byte/boolean from byte[]/boolean[] array (inline)
            0x33 => {
                if self.try_array_load_through_homes(code, op, pc, insn_starts) {
                    // The element is held sign-extended: an `i2l` next is a no-op.
                    let next = self.skip_redundant_i2l(code, code_len, pc + 1, branch_targets);
                    return WalkStep::Next(next);
                }
                let index_slot = self.pop_stack();
                let array_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-9 HIGH fix: NPE on null array (JVMS §baload).
                self.emit_null_check_array_load_at(code, pc, insn_starts);
                self.emit_bounds_check(pc);
                self.emit_byte_aload_regs();
                self.push_from_rax();
                pc = self.skip_redundant_i2l(code, code_len, pc + 1, branch_targets);
            }

            // caload — load char from char[] array (inline, 2 bytes/element, zero-extend)
            0x34 => {
                if self.try_array_load_through_homes(code, op, pc, insn_starts) {
                    // The element is held sign-extended: an `i2l` next is a no-op.
                    let next = self.skip_redundant_i2l(code, code_len, pc + 1, branch_targets);
                    return WalkStep::Next(next);
                }
                let index_slot = self.pop_stack();
                let array_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-9 HIGH fix: NPE on null array (JVMS §caload).
                self.emit_null_check_array_load_at(code, pc, insn_starts);
                self.emit_bounds_check(pc);
                self.emit_char_aload_regs();
                self.push_from_rax();
                pc = self.skip_redundant_i2l(code, code_len, pc + 1, branch_targets);
            }

            // saload — load short from short[] array (inline, 2 bytes/element, sign-extend)
            0x35 => {
                if self.try_array_load_through_homes(code, op, pc, insn_starts) {
                    // The element is held sign-extended: an `i2l` next is a no-op.
                    let next = self.skip_redundant_i2l(code, code_len, pc + 1, branch_targets);
                    return WalkStep::Next(next);
                }
                let index_slot = self.pop_stack();
                let array_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-9 HIGH fix: NPE on null array (JVMS §saload).
                self.emit_null_check_array_load_at(code, pc, insn_starts);
                self.emit_bounds_check(pc);
                self.emit_short_aload_regs();
                self.push_from_rax();
                pc = self.skip_redundant_i2l(code, code_len, pc + 1, branch_targets);
            }

            // iastore — store int to int[] array (inline)
            0x4f => {
                let val_slot = self.pop_stack();
                let index_slot = self.pop_stack();
                let array_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-8 CRIT fix: NPE on null array (JVMS §iastore).
                self.emit_null_check_array_store_at(code, pc, insn_starts);
                self.emit_bounds_check(pc);
                self.load_slot_to_reg(RDX, val_slot);
                self.emit_int_astore_regs();
                self.emit_gpu_input_cache_barrier();
                pc += 1;
            }

            // aastore — store a reference into a reference array.
            //
            // Lowered INLINE (null check, bounds check, covariance check,
            // SATB pre-write barrier, store, card mark). The single
            // call-out is `jit_aastore_type_check` (vm/src/jit/helpers.rs),
            // because answering the JVMS §6.5 covariance question needs the
            // class manager. The complete-opcode helper `jit_aastore` is
            // NOT reached from here.
            //
            // ## Why the covariance check is emitted at all
            //
            // This note used to read "the current `jit_aastore` helper does
            // NOT enforce the ASE check (the interpreter does it via
            // `set_array_element`). This inline path matches the helper's
            // behavior exactly — no regression." That premise was TRUE when
            // R20 / HIGH-5 (docs/PRESENTATION.md) replaced the helper call
            // with the inline store, and was FALSIFIED later — silently —
            // when the JVMS §aastore covariance check landed inside
            // `jit_aastore`, because this path had already stopped calling
            // that helper and a premise stated in a comment is not a
            // compile-time link. From that moment the compiled tier
            // performed every reference array store unconditionally while
            // the interpreter refused the illegal ones, and nothing failed
            // to build.
            //
            // The consequence is not a wrong answer, it is heap type
            // confusion: `Object[] a = new String[1]; a[0] = anInteger;`
            // leaves an `Integer` inside a `String[]`, so a later
            // `aaload`-and-use reads a `String`-typed reference to an
            // `Integer` with no cast to catch it. Under a precise GC that
            // is a memory-safety-relevant corruption, not an etiquette
            // problem — and it is TIER-DEPENDENT: correct for the first
            // ~500 executions and wrong once the method tiers up.
            // `RExceptions` read `cold=[java.lang.Integer] hot=[no-throw]`
            // at i≈500, i.e. the tier-parity assertion caught it the moment
            // the method tiered up. See
            // docs/feature-designs/jit-ir-optimizations.md.
            //
            // The claim that an inline ASE check "needs type-narrowing
            // infrastructure" is also not so: type narrowing is what would
            // let a check be ELIDED, not what makes one correct. And the
            // rule now lives in exactly ONE body — `aastore_store_is_refused`
            // in vm/src/jit/helpers.rs, shared by `jit_aastore_type_check`
            // and `jit_aastore` — so the inline lowering and the full helper
            // can never again enforce different rules.
            //
            // ## Ordering — JVMS §6.5 is NPE → AIOOBE → ASE
            //
            // Not stylistic. Putting the covariance check ahead of the
            // bounds check reproduces the exact divergence
            // `RArrayStoreTiers` s15 caught in the interpreter fast path:
            // ASE reported for a past-the-end index.
            //
            // `flush_scratch_registers` first: it rewrites every
            // register-resident (`Scratch`/`Xmm`) stack slot to a frame
            // slot, so every `load_slot_to_reg` below reads from memory and
            // cannot clobber another's source register regardless of ABI
            // (`ARG_REGS` is RCX/RDX/R8/R9 on Windows, RDI/RSI/RDX/RCX on
            // SysV).
            //
            // A helper call clobbers the caller-saved registers, so
            // `array_slot` / `index_slot` / `val_slot` are RE-LOADED after
            // the check and again after the SATB barrier. Hoisting those
            // loads above either call is silently wrong.
            //
            // 0x53 is a one-byte opcode, so
            // `emit_post_invoke_exception_check` keeps THIS pc as the throw
            // pc, which is what the handler `[start_pc, end_pc)` range test
            // needs (see the note at that function).
            //
            // ## Why this arm must force the dispatch-aware entry
            //
            // `jit_aastore_type_check` builds its `ArrayStoreException`
            // through `jit_thread_mut()`, which only the dispatch-aware
            // entry sets (`vm/src/runtime/interpreter/jit_bridge.rs` — the
            // `!compiled.has_dispatch` arm skips `set_jit_thread`). Without
            // it the check fails open (its documented last resort) and the
            // illegal store proceeds. `emitted_aastore_throw` below is what
            // forces that entry; `x64/driver.rs` reads it alongside
            // `emitted_checkcast_throw`, for the same reason.
            0x53 => {
                // The gate census. `AASTORE_SITES_WALKED` is the
                // denominator that makes the expected ZERO on
                // `AASTORE_ZGC_GATE_FALLBACKS` readable as "consulted and
                // correctly declined" rather than "never reached"; see the
                // doc on those statics.
                AASTORE_SITES_WALKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let barrier_armed = zgc_read_barrier_blocks_inline_fields();
                // zt t32 (successor page section 7,
                // `CRATONVM_ZGC_INLINE_COLORED_STORES`): on an armed
                // colored-store heap with ZGC's fused gate, the element is
                // stored inline -- the colored word -- while both gate bytes
                // are clear (no mark: the good color is `Remapped`), and every
                // other answer takes the whole `jit_aastore`, never the inline
                // SATB arm below (it would log a colored old element).
                let colored_aastore = if barrier_armed && !no_aastore_barrier_gate() {
                    crate::x64::licm::zgc_colored_inline_store_base().filter(|_| {
                        gated_ref_store_enabled()
                            && self.ref_store_gates().is_some()
                            && !self.inline_card_mark_available()
                            && crate::x64::zgc_fused_ref_store_gate_sites_of(&self.helpers)
                                .is_some()
                            && self.helpers.aastore != 0
                            && !(self.precise_exception_frames && self.pc_is_protected(pc))
                    })
                } else {
                    None
                };
                let gate_taken =
                    barrier_armed && colored_aastore.is_none() && !no_aastore_barrier_gate();
                if barrier_armed && !gate_taken && colored_aastore.is_none() {
                    AASTORE_ZGC_GATE_SUPPRESSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                if dbg_aastore_barrier_gate() {
                    let verdict = if gate_taken {
                        "helper-fallback"
                    } else if barrier_armed {
                        "inline/suppressed-by-kill-switch"
                    } else {
                        "inline/barrier-not-armed"
                    };
                    eprintln!("[aastore-gate] pc={pc} armed={barrier_armed} verdict={verdict}");
                }
                self.flush_scratch_registers();
                let val_slot = self.pop_stack();
                let index_slot = self.pop_stack();
                let array_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-8 CRIT fix: NPE on null array (JVMS §aastore).
                // Records a null-check stub.
                self.emit_null_check_array_store_at(code, pc, insn_starts);
                // AIOOBE. Records a bounds-check stub.
                self.emit_bounds_check(pc);

                // ## The ZGC read-barrier coverage gate
                //
                // Everything below this point touches the element SLOT as
                // raw machine code: `emit_ref_aload_regs` reads the old
                // reference for the SATB snapshot and `emit_ref_astore_regs`
                // writes the new one. Both emitters understand compressed
                // oops themselves (`emit_narrow_ref_aload_regs` /
                // `emit_narrow_ref_astore_regs` encode and decode the 4-byte
                // `(addr - base) >> 3` form), which is exactly why the
                // missing gate here stayed invisible: the narrow half of
                // `narrow_oops_block_inline_fields()` is genuinely handled,
                // so nothing ever went wrong under narrow oops and nobody
                // looked at the other half.
                //
                // The other half is `zgc_read_barrier_blocks_inline_fields()`
                // and it is NOT handled. Under an armed ZGC cycle a
                // reference slot holds `Z_COLORED_TAG (bit 63) | colour
                // (bits 42..=46) | 42-bit offset`, which is not a machine
                // pointer at all:
                //   * the inline LOAD would read that word with no colour
                //     test and hand it to `jit_satb_pre_write_barrier` as if
                //     it were a pointer -- a Category-A read-barrier hole,
                //     and one that is not even among the nine emission
                //     points of `zgc-jit-load-barrier.md` 2.3;
                //   * the inline STORE would write a PLAIN pointer into a
                //     slot the barrier next classifies with
                //     `classify_bad_masked`, which reads an uncoloured word
                //     as `Good` and truncates it to 42 bits -- silently, per
                //     that function's own doc -- and can clobber a heal that
                //     was concurrently in flight.
                //
                // Be precise about what this is NOT. It is not the
                // `write_ref_slot` data race of
                // `.agent-requests/A16-vm-stores.txt` section 1: an aligned
                // qword `mov` emitted by the JIT is not a Rust memory access
                // at all, and on x86-64 it is architecturally atomic against
                // a `lock cmpxchg`. Nothing here can tear, and no Rust UB is
                // in play. What is wrong is COVERAGE -- the rule
                // `classify_bad_masked` states for itself, that a slot is
                // either fully barriered on BOTH the read and the write side
                // or is never handed to it at all.
                //
                // # Why the ZGC disjunct only, and not the whole
                // # `narrow_oops_block_inline_fields()` predicate
                //
                // Because the narrow half is already covered here, and
                // refusing it would be a pure throughput regression on about
                // the hottest reference-store opcode there is: every
                // `Object[]` store in every narrow-oops run would take a
                // helper call to buy nothing. The two compact-field arms
                // (`getfield` above, reference `putfield` below) use the
                // combined predicate because THEIR inline emitters bake an
                // 8-byte access at a fixed offset and would read the wrong
                // width under narrow oops; that hazard does not exist at
                // this site. ZGC and compressed oops are mutually refused at
                // VM init in any case (A9's P1), so the two disjuncts can
                // never both be true and splitting them loses nothing.
                //
                // # Why this changes nothing on a default run
                //
                // `cratonvm_types::zgc_read_barrier_armed()` is a
                // process-global flag whose only writer is
                // `ZgcRealHeap::set_barrier_color`, which has no non-test
                // caller; `RELOCATION_REQUESTED` is a pinned `false` in
                // `vm/src/vm/vm_init.rs`, and `barrier_good_mask` never
                // leaves `Z_REMAPPED`. So `gate_taken` is false in every
                // shipping configuration and the bytes this arm emits are
                // identical to what it emitted before. The cost is one
                // relaxed load of that flag per compiled `aastore` SITE --
                // not per execution.
                //
                // What it does close is a claim that was already being made
                // elsewhere: `zgc_codegen_honours_read_barrier()` returns a
                // constant `true`, and its mechanism 2 says "every inline
                // compact-field site is gated on
                // `narrow_oops_block_inline_fields`". This site was not, so
                // that justification was false here; `zgc_relocation_permitted`
                // reads it, which is how an unclosed hole would have become
                // a relocating cycle handing JIT code stale pointers.
                //
                // # The residual this cannot close
                //
                // An emission-time gate cannot reach code that is ALREADY
                // compiled, so arming must still happen where no Java thread
                // is inside compiled code, i.e. at a safepoint. That
                // obligation is recorded on `ZgcRealHeap::set_barrier_color`
                // and is unchanged by this gate.
                //
                // (The `if gate_taken` branch itself sits just after the
                // covariance check below, so both routes run the check.)
                //
                // JVMS §aastore covariance check, BEFORE anything mutates:
                // on a refusal no element may be written and no barrier may
                // run. `jit_aastore_type_check` answers 0 (legal) or the
                // i64::MIN sentinel, having stashed the
                // ArrayStoreException; `emit_post_invoke_exception_check`
                // routes the sentinel through the same drain
                // `jit_checkcast`'s ClassCastException uses.
                //
                // A null value is legal for every reference array, so it
                // branches over the call entirely — `arr[i] = null` keeps
                // costing a test and a not-taken jump. So does a plain value
                // whose class IS the array's component class (the fast path
                // below). Everything else pays one call, which is the price
                // of the JVMS rule.
                self.load_slot_to_reg(RDX, val_slot);
                self.buf.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
                self.buf.emit(&[0x0F, 0x84]); // JZ rel32 -> past the call
                let ase_skip_patch = self.buf.pos();
                self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                // ## The exact-component fast path (r11 x64gen)
                //
                // A reference array's header class id IS its component class
                // (`typecheck::array_descriptor_of`), and a PLAIN object's is
                // its own class. So a plain value whose class id equals the
                // array's is an instance of the component class exactly, and
                // the store is legal with no hierarchy question to ask —
                // `String` into `String[]`, `Node` into `Node[]`, every
                // `E[] elements` of a homogeneous collection. That store used
                // to pay the full helper call (blind spill, oop map, sentinel
                // check) to be told yes.
                //
                // An ARRAY value is screened out first (its header carries its
                // COMPONENT's id, so `String[]` stored into `String[][]` would
                // compare `String` against `String[]` — unequal, fine — but
                // `String[]` into `String[]` would compare `String` against
                // `String` and wrongly accept; the KIND_TAGS test is the same
                // one the checkcast/instanceof guards use). Anything else takes
                // the helper exactly as before; this only adds a way to NOT
                // call it for an answer that cannot be "refuse".
                let mut exact_hit: Vec<usize> = Vec::new();
                {
                    const KIND_DISP: u8 =
                        crate::x64::disp::disp8_const(cratonvm_types::KIND_TAGS_BYTE_OFFSET as i64)
                            as u8; // Cast: checked disp8
                                   // TEST BYTE [RDX + KIND_TAGS], KIND_TAG_BYTE_MASK (F6 /0 ib,
                                   // ModRM 0x42 = mod01 disp8 /0 rm=RDX): the kind bits only,
                                   // since the 8-byte header shares the byte with mark state.
                    self.buf
                        .emit(&[0xF6, 0x42, KIND_DISP, cratonvm_types::KIND_TAG_BYTE_MASK]);
                    let not_plain = self.emit_jcc_rel32_patch(0x85); // JNE → helper
                                                                     // Cast: a fixed header offset, as the checkcast guard uses it.
                    let cid_off = self.helpers.class_id_offset_in_obj as i32;
                    self.load_slot_to_reg(R11, array_slot);
                    self.emit_mov_r32_mem_disp32(R10, RDX, cid_off); // value's class id
                                                                     // Class id 0 never takes the fast path: it is what a
                                                                     // primitive array and a component the class manager cannot
                                                                     // name both carry, so an equality there proves nothing.
                    self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D, R10D
                    let id_zero = self.emit_jcc_rel32_patch(0x84); // JZ → helper
                                                                   // CMP R10D, DWORD [R11 + disp32]: REX.RB (45), 3B /r,
                                                                   // ModRM 0x93 = mod10 reg=R10(010) rm=R11(011) — R11 is not
                                                                   // an RSP/R12-class base, so no SIB.
                    self.buf.emit(&[0x45, 0x3B, 0x93]);
                    self.buf.emit(&cid_off.to_le_bytes());
                    exact_hit.push(self.emit_jcc_rel32_patch(0x84)); // JE → past the call
                    // JIT r21 wave 2 (lane typecheck, TYPECHECK21-5,
                    // `CRATONVM_JIT_SP_AASTORE_DISPLAY_ARM`, opt-in): the IR
                    // tier's store-check proof (`aastore_display_arm_bytes`):
                    // a value whose class is a subclass of the component
                    // class, or implements the component interface, stores
                    // without the call. Only a YES; every other outcome calls
                    // the helper below unchanged. The arm writes RCX, RDX and
                    // R10 (and the flags); after `flush_scratch_registers` none
                    // holds an operand slot, and every path past the join
                    // reloads RAX / RCX / RDX from their slots. A component id
                    // of 0 (`Object[]`) is answered by `java/lang/Object`'s
                    // own display, or declines.
                    if let Some(cell) = sp_aastore_display_cell(&self.direct_helpers) {
                        self.buf.emit(&[0x44, 0x89, 0xD2]); // MOV EDX, R10D (value id)
                        self.buf.emit(&[0x41, 0x8B, 0x8B]); // MOV ECX, [R11 + disp32] (component id)
                        self.buf.emit(&cid_off.to_le_bytes());
                        let (bytes, edges, hits) = crate::ir_lower::aastore_display_arm_bytes(cell);
                        let at = self.buf.pos();
                        self.buf.emit(&bytes);
                        exact_hit.extend(hits.into_iter().map(|h| at + h));
                        for e in edges {
                            self.patch_rel32_to_here(at + e);
                        }
                        if crate::x64::dbg_jitc_enabled() {
                            eprintln!(
                                "[cratonvm-jitc] sp aastore display arm: {} pc={pc}",
                                self.method_key
                            );
                        }
                    }
                    self.patch_rel32_to_here(not_plain);
                    self.patch_rel32_to_here(id_zero);
                }
                // Args: (vm_ptr, array_ptr, value_ptr).
                self.emit_load_vm_context(ARG_REGS[0]);
                self.load_slot_to_reg(ARG_REGS[1], array_slot);
                self.load_slot_to_reg(ARG_REGS[2], val_slot);
                // The refusal path allocates (it builds the throwable), so
                // spill and publish an oop map exactly as `checkcast` does.
                self.emit_pre_safepoint_spill();
                // (vm_ptr, array_ptr, value_ptr) -> 0 | i64::MIN. Three registers, not
                // `aastore`'s four — the index is not passed, because the check is
                // about the element TYPE and not the slot.
                cratonvm_jit_api::assert_helper_call_shape!(
                    "aastore_type_check",
                    int_args = 3,
                    returns_value = true
                );
                self.emit_call_absolute(self.helpers.aastore_type_check);
                self.emit_oop_map_for_safepoint();
                // `b'V'` is the OPCODE's return descriptor, which is what
                // this parameter documents; it selects the plain
                // `CMP RAX, i64::MIN; JE bail`, correct because the helper
                // returns only `0` or the sentinel — RAX here is a defined
                // value, not the undefined RAX a `-> ()` helper leaves.
                self.emit_post_invoke_exception_check(b'V');
                self.emitted_aastore_throw = true;
                {
                    let here = self.buf.pos() as i32;
                    let rel = here - (ase_skip_patch as i32 + 4);
                    self.buf.try_patch_i32(ase_skip_patch, rel).ok();
                }
                for p in exact_hit {
                    self.patch_rel32_to_here(p);
                }
                // (r9w3 x64obj3) The ZGC gate's helper route runs AFTER the
                // covariance check above, exactly as the IR tier's
                // `Op::ArrayStore(Ref)` arm does. `jit_aastore` is void: its
                // own refusal only stashes the ArrayStoreException for the
                // interpreter's post-JIT drain, so the compiled body used to
                // CONTINUE past a refused store and run the rest of the
                // method with the exception pending (side effects the JVMS
                // says never happen). With the check first a refused store
                // leaves through `emit_post_invoke_exception_check` before
                // the void call, which then only re-checks a legal store.
                if gate_taken {
                    // The fallback is `jit_aastore`, which performs the
                    // whole opcode -- NPE, AIOOBE, the SAME
                    // `aastore_store_is_refused` covariance rule the inline
                    // path calls, the SATB pre-read, the store and the post
                    // barrier -- through `read_ref_slot` / `write_ref_slot`,
                    // the single chokepoint the barrier is being plumbed
                    // into. The design doc calls the helper-CALL arms "the
                    // barrier's cheap escape hatch": correct today, with
                    // inline emission a throughput optimisation on top
                    // rather than a correctness prerequisite.
                    //
                    // The null and bounds checks above are left in place and
                    // are therefore emitted twice on this path. That is
                    // deliberate: they are the well-exercised JIT stubs, the
                    // duplicate is a compare on a path that cannot execute
                    // today, and removing them would make the two arms
                    // differ in more than the one variable being changed.
                    //
                    // Not inside a protected range under precise exception
                    // frames (r11 wave 8). `jit_aastore` is void and publishes
                    // no frame, so whatever it raised would reach a handler
                    // that reads a non-parameter local with nothing recorded.
                    // The inline checks above make that unreachable for the
                    // three JVMS exceptions (each publishes, and each already
                    // ran), but refusing here means RBC.6's admission of this
                    // opcode never depends on that argument, nor on the gate
                    // staying unarmed. See `precise_array_access_enabled`.
                    if self.precise_exception_frames && self.pc_is_protected(pc) {
                        crate::note_jit_bail_site_at(
                            "aastore-zgc-fallback-in-protected-range",
                            pc,
                            0x53,
                        );
                        return WalkStep::Return(false);
                    }
                    if self.helpers.aastore == 0 {
                        // An unwired helper table (only the unit-test
                        // sentinel tables leave this zero). Refuse the
                        // method rather than emit a call to address 0 --
                        // and, more to the point, rather than fall through
                        // to the inline sequence, which with the barrier
                        // armed is precisely the defect this gate exists to
                        // avoid. Failing closed is the only correct answer.
                        crate::note_jit_bail_site_at("aastore-zgc-barrier-no-helper", pc, 0x53);
                        return WalkStep::Return(false);
                    }
                    AASTORE_ZGC_GATE_FALLBACKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // Args: (vm_ptr, array_ptr, index, val). Loaded in
                    // ARG_REGS order after `flush_scratch_registers`, so
                    // every source is a frame slot (or a callee-saved
                    // register, which is never an `ARG_REGS` member) and no
                    // load can clobber a later one's source on either ABI.
                    self.emit_load_vm_context(ARG_REGS[0]);
                    self.load_slot_to_reg(ARG_REGS[1], array_slot);
                    self.load_slot_to_reg(ARG_REGS[2], index_slot);
                    self.load_slot_to_reg(ARG_REGS[3], val_slot);
                    // `jit_aastore` builds an ArrayStoreException on its
                    // refusal path, so it allocates: spill and publish an
                    // oop map exactly as the inline path does around
                    // `aastore_type_check`.
                    self.emit_pre_safepoint_spill();
                    // ABI-3 — `emit_call_absolute` takes a bare address, so nothing but
                    // this assertion connects the four registers written above to the
                    // helper's declared arity. `returns_value = false` is the load-bearing
                    // half here: `jit_aastore` returns `()`, which is exactly why the arm
                    // below refuses to emit a post-invoke sentinel check, and the two
                    // statements would otherwise be free to drift apart.
                    cratonvm_jit_api::assert_helper_call_shape!(
                        "aastore",
                        int_args = 4,
                        returns_value = false
                    );
                    self.emit_call_absolute(self.helpers.aastore);
                    self.emit_oop_map_for_safepoint();
                    // Deliberately NO `emit_post_invoke_exception_check`.
                    // `jit_aastore` returns `()`, so RAX is UNDEFINED on
                    // return and the `CMP RAX, i64::MIN; JE bail` that check
                    // emits would fire on whatever the helper happened to
                    // leave there. Its exceptions travel by the
                    // pending-signal channel instead
                    // (`set_jit_pending_npe_action`, `JIT_SIGNALS.aioobe`,
                    // `set_jit_pending_exception`), which the interpreter's
                    // post-JIT path drains on every return -- the documented
                    // contract of the void helper, and the reason the inline
                    // arm's `b'V'` note is careful to say that ITS RAX is a
                    // defined value.
                    //
                    // `emitted_aastore_throw` is still set: the helper
                    // reaches the ArrayStoreException through
                    // `jit_thread_mut()` exactly as `jit_aastore_type_check`
                    // does, so it needs the dispatch-aware entry for the
                    // same reason, and without it the check fails open and
                    // the illegal store proceeds.
                    self.emitted_aastore_throw = true;
                    pc += 1;
                    return WalkStep::Next(pc);
                }
                // The call clobbers the scratch registers; re-establish
                // RAX=array / RCX=index for the SATB load below.
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                if let Some(base) = colored_aastore {
                    // zt t32: the fused gate (both bytes clear: no mark, no
                    // old-object barrier), the colored element store, and the
                    // whole `jit_aastore` for every other answer.
                    let Some(word) = crate::x64::zgc_fused_ref_store_gate_sites_of(&self.helpers)
                    else {
                        // Admitted with a fused gate, so this cannot miss;
                        // if it ever does, refuse the method (the walk's
                        // error path) rather than panic inside the compiler.
                        crate::note_jit_bail_site_at("aastore-zgc-fused-gate-missing", pc, 0x53);
                        return WalkStep::Return(false);
                    };
                    self.emit_zgc_fused_gate_cmp(word);
                    let busy = self.emit_jcc_rel32_patch(0x85); // JNE → jit_aastore
                    self.load_slot_to_reg(RDX, val_slot);
                    crate::x64::licm::emit_zgc_colored_store_encode_rdx(&mut self.buf, base);
                    self.emit_ref_astore_regs();
                    let done = self.emit_jmp_rel32_patch();
                    self.patch_rel32_to_here(busy);
                    self.emit_load_vm_context(ARG_REGS[0]);
                    self.load_slot_to_reg(ARG_REGS[1], array_slot);
                    self.load_slot_to_reg(ARG_REGS[2], index_slot);
                    self.load_slot_to_reg(ARG_REGS[3], val_slot);
                    self.emit_pre_safepoint_spill();
                    cratonvm_jit_api::assert_helper_call_shape!(
                        "aastore",
                        int_args = 4,
                        returns_value = false
                    );
                    self.emit_call_absolute(self.helpers.aastore);
                    self.emit_oop_map_for_safepoint();
                    // As on the gate's helper route: no post-invoke check (the
                    // helper is void), and the dispatch-aware entry it needs.
                    self.emitted_aastore_throw = true;
                    self.patch_rel32_to_here(done);
                    if dbg_jitc_enabled() {
                        eprintln!(
                            "[cratonvm-jitc] sp zgc-colored-aastore pc={pc} method={}",
                            self.method_key
                        );
                    }
                    pc += 1;
                    return WalkStep::Next(pc);
                }
                // ## Gated barriers (r9-ea)
                //
                // Both barrier CALLs below used to be unconditional, so every
                // compiled reference-array store paid two helper transitions on
                // top of the covariance check — while `putfield` of a reference
                // (`emit_gated_compact_ref_putfield`) has skipped exactly these
                // two calls since 2026-09-02 on the collector's published gates.
                // The gates are the helpers' OWN first tests, read from the same
                // bytes (`gc::gen_heap::JitRefStoreGates`), and they are
                // properties of the RECEIVER's header and of the collector, not
                // of whether the receiver is an object or an array:
                //
                // * `pre_active == 0` — `jit_satb_pre_write_barrier` →
                //   `satb_barrier` returns without logging. It is only ever
                //   armed inside a stop-the-world pause, and nothing between
                //   this test and the store below is a safepoint, so the
                //   answer cannot go stale in between for a thread that
                //   ARRIVES at the pause. A thread the BUG-03 takeover freezes
                //   in between is covered by the activating pause scanning
                //   what it holds (the array, in RAX or its frame slot): Generational
                //   `initial_mark_with_frozen`, G1 `scan_frozen_held_objects`
                //   (gci i12/wb; the same holds for G1's ungated arm below,
                //   whose helper answers "inactive" before the store). When it
                //   is armed the old element is loaded and logged exactly as
                //   before.
                // * a null value, a receiver the published mask/floor calls
                //   young, or `post_active == 0` — `jit_write_barrier` returns
                //   without recording an edge. A humongous array is allocated
                //   in old generation WITH `GC_FLAG_OLD_GEN`, so the mask never
                //   calls it young.
                //
                // Same lever as the putfield arm (`CRATONVM_JIT_GATED_REF_STORE=0`
                // restores both unconditional calls), and nothing changes when
                // no collector published a plan (G1): `ref_store_gates()` is
                // `None` and the calls stay unconditional.
                let aastore_gates = if gated_ref_store_enabled() {
                    self.ref_store_gates()
                } else {
                    None
                };
                // r21 barriers (BARRIERS21-1, `CRATONVM_JIT_SP_G1_GATED_AASTORE`,
                // default ON since r23): G1 publishes no plan, but its SATB pre-gate byte and its
                // region table give the same two skips. The SATB one is the gate
                // below. The post one is the F-08 filter after the store. Both
                // dismiss only calls whose helper would have returned
                // (`objects::g1_aastore_plan_from` gives the argument).
                let g1_aastore = if aastore_gates.is_none()
                    && gated_ref_store_enabled()
                    && super::objects::sp_g1_gated_aastore_enabled()
                {
                    super::objects::g1_aastore_plan_of(&self.helpers)
                } else {
                    None
                };
                // zs s5/bt2: the fused gate (ZGC's age-floor plan). One 16-bit
                // compare of the adjacent `pre_active` / `post_active` bytes;
                // both zero is sufficient for every arm below that reaches the
                // store without a barrier (pre gate clear; then a null value,
                // a young receiver or a clear post gate), so on that answer
                // the element is stored here and the opcode is done. Any other
                // word runs the unchanged sequence. RAX = array, RCX = index on
                // both edges (the compare touches R11 and the flags only).
                // Never with the inline card arm (the generational mask plan,
                // which the fused word excludes anyway).
                let fused_done = match aastore_gates
                    .and_then(|_| crate::x64::zgc_fused_ref_store_gate_sites_of(&self.helpers))
                {
                    Some(word) if !self.inline_card_mark_available() => {
                        self.emit_zgc_fused_gate_cmp(word);
                        let busy = self.emit_jcc_rel32_patch(0x85); // JNE → full sequence
                        self.load_slot_to_reg(RDX, val_slot);
                        self.emit_ref_astore_regs();
                        let done = self.emit_jmp_rel32_patch();
                        self.patch_rel32_to_here(busy);
                        Some(done)
                    }
                    _ => None,
                };
                let pre_gate = aastore_gates
                    .map(|(pre, _, _)| pre)
                    .or(g1_aastore.map(|p| p.pre));
                // JE → marking idle, no SATB work (the same bytes for both shapes).
                let pre_skip = pre_gate.map(|pre| self.emit_sp_aastore_satb_gate_skip(pre));
                // Round-7 fix (CRIT, UAF in JIT): SATB pre-write barrier.
                // Inline-load the OLD reference at the slot and pipe it
                // through `jit_satb_pre_write_barrier(vm_ptr, old_ref)`
                // BEFORE the inline store overwrites it. Without this, a
                // still-live reference overwritten by JIT code during
                // concurrent marking would be silently dropped by the
                // marker → use-after-free on the next mixed evacuation
                // (audit: round7-gc.md §1). With a published plan the
                // pre gate above (`pre_skip`) skips both the load and the
                // call while no mark runs, so this arm runs only during a
                // mark (zt t9/ca: the old text here described the
                // pre-gate era, when every store loaded and called and the
                // helper did the idle test itself).
                //
                // `emit_ref_aload_regs` clobbers RAX with the loaded value;
                // the array, index and value are reloaded from their slots
                // after the call.
                //
                // zt t9/ca (Q4, ZGC's age-floor plan only): the pre arm's
                // run-time census under `CRATONVM_DBG_SP_REF_STORE_TRACE=1`
                // (`ENTERED` here, `CALLED` in front of the call; R11 and the
                // flags only, both dead at either point), and under
                // `CRATONVM_JIT_ZGC_SP_AASTORE_NULL_OLD_SKIP=1` a null old
                // element skips the call (`zgc_sp_aastore_null_old_skip_enabled`).
                let zgc_pre_arm = pre_skip.is_some()
                    && super::objects::zgc_sp_aastore_pre_arm_of(&self.helpers);
                if zgc_pre_arm {
                    use crate::metrics::{SP_AASTORE_SATB_PRE, SP_AASTORE_SATB_PRE_ENTERED};
                    self.emit_ref_store_path_trace(
                        &SP_AASTORE_SATB_PRE[SP_AASTORE_SATB_PRE_ENTERED],
                    );
                }
                self.emit_ref_aload_regs(); // RAX = OLD ref value
                let skip_null_old =
                    zgc_pre_arm && super::objects::zgc_sp_aastore_null_old_skip_enabled();
                let null_old = if skip_null_old {
                    self.emit_test_r64_r64(RAX);
                    Some(self.emit_jcc_rel32_patch(0x84)) // JZ → past the call
                } else {
                    None
                };
                if zgc_pre_arm {
                    use crate::metrics::{SP_AASTORE_SATB_PRE, SP_AASTORE_SATB_PRE_CALLED};
                    self.emit_ref_store_path_trace(
                        &SP_AASTORE_SATB_PRE[SP_AASTORE_SATB_PRE_CALLED],
                    );
                }
                self.emit_load_vm_context(ARG_REGS[0]);
                self.emit_mov_reg_reg(ARG_REGS[1], RAX);
                // (vm_ptr, old_ref) -> ().
                cratonvm_jit_api::assert_helper_call_shape!(
                    "satb_pre_write_barrier",
                    int_args = 2,
                    returns_value = false
                );
                self.emit_call_absolute(self.helpers.satb_pre_write_barrier);
                if let Some(skip) = pre_skip {
                    self.patch_rel32_to_here(skip);
                }
                if let Some(skip) = null_old {
                    self.patch_rel32_to_here(skip);
                }
                // Reload array / index / new value (helper call may have
                // clobbered scratch registers including RAX, RCX, RDX).
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                self.load_slot_to_reg(RDX, val_slot);
                // gen r4w4/cards4: the inline generational barrier
                // (`CRATONVM_JIT_INLINE_CARD_MARK`), PRE half. On an
                // element-precise card table it checks — and its slow arm
                // dirties — the ELEMENT's card, which is what
                // `set_array_element` marks and `scan_dirty_cards_inner`'s
                // window arm reads; otherwise the array's header card.
                let inline_card = self.inline_card_mark_available();
                let precise = self.gen_card_view_precise_arrays();
                // gcj j2/jt2: post-only (no PRE, no lean POST), one reading.
                // RAX (the array) and RDX stay live from the store to the card
                // byte, which is what the take-over's completion relies on.
                let post_only = self.post_only_card_site(inline_card);
                // gci i23/lc: one reading for both halves. The index reload and
                // `emit_ref_astore_regs` (R11 for the narrow form) keep R10.
                let lean =
                    inline_card && !post_only && super::objects::jit_lean_card_post_enabled();
                if post_only {
                    self.note_post_only_card_site();
                } else if inline_card {
                    // gcj j1/jt (i23-lc review finding 2): the index is
                    // reloaded from its slot AFTER the PRE barrier, whose slow
                    // arm is a call, so the slot must survive one -- a frame
                    // word or a callee-saved register, never the R8/R9 scratch
                    // cache (the PRE reload list asserts the same of the array
                    // and value slots).
                    debug_assert!(matches!(
                        index_slot,
                        StackSlot::Frame(_) | StackSlot::CalleeSaved(_)
                    ));
                    let mark = if precise {
                        // RCX: index → element address. Restored below.
                        self.emit_ref_element_address(RCX, RAX, RCX);
                        RCX
                    } else {
                        RAX
                    };
                    self.emit_gen_card_barrier_pre(
                        RAX,
                        RDX,
                        mark,
                        &[(RAX, array_slot), (RDX, val_slot)],
                        lean,
                    );
                    // The fast path left RCX = the element address, the slow
                    // path clobbered it: the store needs the index either way.
                    self.load_slot_to_reg(RCX, index_slot);
                }
                // Inline store: MOV QWORD [RAX + RCX*8 + HEADER_SIZE], RDX
                // (RAX, RCX and RDX survive it, the compressed form included:
                // that one encodes through R11).
                self.emit_ref_astore_regs();
                // Post-store publication. G1/ZGC retain their collector-specific
                // helper (no card view is published for them).
                if inline_card {
                    // POST half of the inline generational barrier. gci i23/lc:
                    // under `lean` PRE's card (still dirty) answers it before the
                    // element address is even recomputed.
                    let hit = if lean {
                        Some(self.emit_gen_card_lean_post_head())
                    } else {
                        None
                    };
                    let mark = if precise {
                        self.emit_ref_element_address(RCX, RAX, RCX);
                        RCX
                    } else {
                        RAX
                    };
                    self.emit_gen_card_barrier(RAX, RDX, mark, &[]);
                    if let Some(hit) = hit {
                        self.patch_rel32_to_here(hit);
                    }
                } else if let Some((_, post, floor)) = aastore_gates {
                    // The gated post barrier: every skip lands after the call.
                    let mut done: Vec<usize> = Vec::new();
                    // A null value records no edge.
                    self.emit_test_r64_r64(RDX);
                    done.push(self.emit_jcc_rel32_patch(0x84)); // JZ → done
                                                                // The receiver's flags byte (RCX, the index, is dead now).
                    self.emit_movzx_r32_mem8(RCX, RAX, cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32);
                    if let Some(mask) = self.ref_store_post_skip_mask() {
                        self.emit_test_r8_imm8(RCX, mask);
                        done.push(self.emit_jcc_rel32_patch(0x84)); // JZ → young receiver
                    } else {
                        self.emit_mov_imm64_full(R11, floor as i64);
                        self.emit_cmp_r8_mem8(RCX, R11, 0);
                        done.push(self.emit_jcc_rel32_patch(0x82)); // JB → young receiver
                    }
                    self.emit_mov_imm64_full(R11, post as i64);
                    self.emit_cmp_mem8_imm8(R11, 0, 0);
                    done.push(self.emit_jcc_rel32_patch(0x84)); // JZ → no old objects
                                                                // gen r4w4/cards4 (precise-array residual, item 1): the
                                                                // element's slot on an element-precise card table.
                    // zt t7/tg (GN-8, opt-in): on ZGC's age-floor plan, the
                    // element's slot (tagged) so the card can be its granule.
                    if self.ref_store_post_skip_mask().is_none()
                        && !self.gen_card_view_precise_arrays()
                        && super::objects::jit_zgc_element_card_barrier_enabled()
                    {
                        self.emit_zgc_aastore_element_barrier_args(array_slot, index_slot);
                    } else {
                        self.emit_aastore_write_barrier_args(array_slot, index_slot, val_slot);
                    }
                    // (vm_ptr, obj_ptr, val_ptr) -> ().
                    cratonvm_jit_api::assert_helper_call_shape!(
                        "write_barrier",
                        int_args = 3,
                        returns_value = false
                    );
                    self.emit_call_absolute(self.helpers.write_barrier);
                    for d in done {
                        self.patch_rel32_to_here(d);
                    }
                } else if g1_aastore.is_some() {
                    // r21 barriers (BARRIERS21-1): G1's post barrier, filtered.
                    // A null value or a same-region pair records nothing
                    // (`post_write_barrier_rset`'s own first returns). Every
                    // other store calls the lean `jit_g1_post_write_barrier(vm,
                    // array, value)`, which is `G1Collector::write_barrier`'s
                    // one action. RAX (array), RDX (value) and RCX (the index,
                    // dead here) survive the store. The filter clobbers all
                    // three, and the call reloads its arguments from the slots.
                    // G1 has no element-precise card view, so the mark is the
                    // array, as `emit_aastore_write_barrier_args` passes it.
                    self.emit_g1_post_write_barrier_regs(RAX, RDX, RCX, array_slot, val_slot);
                } else {
                    // gen r4w4/cards4: element-precise, as in the gated arm.
                    self.emit_aastore_write_barrier_args(array_slot, index_slot, val_slot);
                    // (vm_ptr, obj_ptr, val_ptr) -> ().
                    cratonvm_jit_api::assert_helper_call_shape!(
                        "write_barrier",
                        int_args = 3,
                        returns_value = false
                    );
                    self.emit_call_absolute(self.helpers.write_barrier);
                }
                if let Some(done) = fused_done {
                    self.patch_rel32_to_here(done);
                }
                pc += 1;
            }

            // lastore — store long to long[] array (inline, 8 bytes/element)
            0x50 => {
                let val_slot = self.pop_stack();
                let index_slot = self.pop_stack();
                let array_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-8 CRIT fix: NPE on null array (JVMS §lastore).
                self.emit_null_check_array_store_at(code, pc, insn_starts);
                self.emit_bounds_check(pc);
                self.load_slot_to_reg(RDX, val_slot);
                self.emit_long_astore_regs();
                self.emit_gpu_input_cache_barrier();
                pc += 1;
            }

            // fastore — store float to float[] array (inline, 4 bytes/element)
            0x51 => {
                let val_slot = self.pop_stack();
                let index_slot = self.pop_stack();
                let array_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-8 CRIT fix: NPE on null array (JVMS §fastore).
                self.emit_null_check_array_store_at(code, pc, insn_starts);
                self.emit_bounds_check(pc);
                match val_slot {
                    StackSlot::Xmm(xmm) => {
                        // MOVSS [RAX + RCX*4 + HEADER_SIZE], XMMn
                        // F3 [REX] 0F 11 ModRM SIB disp8
                        let rex_r = if xmm >= 8 { 0x04u8 } else { 0 };
                        self.buf.emit_byte(0xF3);
                        if rex_r != 0 {
                            self.buf.emit_byte(0x40 | rex_r);
                        }
                        self.buf.emit_byte(0x0F);
                        self.buf.emit_byte(0x11);
                        self.buf.emit_byte(0x44 | ((xmm & 7) << 3)); // ModRM: mod=01, reg=xmm, r/m=SIB
                        self.buf.emit_byte(0x88); // SIB: scale=2(*4), index=RCX, base=RAX
                        self.buf.emit_byte(HEADER_SIZE as u8); // Cast: x86-64 immediate encoding
                    }
                    _ => {
                        self.load_slot_to_reg(RDX, val_slot);
                        self.emit_int_astore_regs();
                    }
                }
                self.emit_gpu_input_cache_barrier();
                pc += 1;
            }

            // dastore — store double to double[] array (inline, 8 bytes/element)
            0x52 => {
                let val_slot = self.pop_stack();
                let index_slot = self.pop_stack();
                let array_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-8 CRIT fix: NPE on null array (JVMS §dastore).
                self.emit_null_check_array_store_at(code, pc, insn_starts);
                self.emit_bounds_check(pc);
                // Optimize: if value is in XMM, use MOVSD to store directly to memory
                match val_slot {
                    StackSlot::Xmm(xmm) => {
                        // MOVSD [RAX + RCX*8 + HEADER_SIZE], XMMn
                        // F2 [REX] 0F 11 ModRM SIB disp8
                        let rex_r = if xmm >= 8 { 0x04u8 } else { 0 };
                        self.buf.emit_byte(0xF2);
                        if rex_r != 0 {
                            self.buf.emit_byte(0x40 | rex_r);
                        }
                        self.buf.emit_byte(0x0F);
                        self.buf.emit_byte(0x11); // MOVSD store direction
                        self.buf.emit_byte(0x44 | ((xmm & 7) << 3)); // ModRM: mod=01, reg=xmm, r/m=SIB
                        self.buf.emit_byte(0xC8); // SIB: scale=3(*8), index=RCX, base=RAX
                        self.buf.emit_byte(HEADER_SIZE as u8); // Cast: x86-64 immediate encoding
                    }
                    _ => {
                        self.load_slot_to_reg(RDX, val_slot);
                        self.emit_long_astore_regs();
                    }
                }
                self.emit_gpu_input_cache_barrier();
                pc += 1;
            }

            // bastore — store byte/boolean to byte[]/boolean[] array (inline)
            0x54 => {
                let val_slot = self.pop_stack();
                let index_slot = self.pop_stack();
                let array_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-8 CRIT fix: NPE on null array (JVMS §bastore).
                self.emit_null_check_array_store_at(code, pc, insn_starts);
                self.emit_bounds_check(pc);
                self.load_slot_to_reg(RDX, val_slot);
                // JVMS §6.5: a `boolean[]` stores `value & 1`, a `byte[]` the
                // low byte, and this one opcode serves both. Skipped when the
                // value is provably 0 or 1 (javac's `a[i] = true/false`),
                // where the two agree. Round 9 wave 8 (arr8).
                if !bastore_value_is_zero_or_one(code, pc, branch_targets, insn_starts) {
                    self.emit_bastore_boolean_mask_regs();
                }
                self.emit_byte_astore_regs();
                self.emit_gpu_input_cache_barrier();
                pc += 1;
            }

            // castore — store char to char[] array (inline, 2 bytes/element)
            0x55 => {
                let val_slot = self.pop_stack();
                let index_slot = self.pop_stack();
                let array_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-8 CRIT fix: NPE on null array (JVMS §castore).
                self.emit_null_check_array_store_at(code, pc, insn_starts);
                self.emit_bounds_check(pc);
                self.load_slot_to_reg(RDX, val_slot);
                self.emit_short_astore_regs();
                self.emit_gpu_input_cache_barrier();
                pc += 1;
            }

            // sastore — store short to short[] array (inline, 2 bytes/element)
            0x56 => {
                let val_slot = self.pop_stack();
                let index_slot = self.pop_stack();
                let array_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-8 CRIT fix: NPE on null array (JVMS §sastore).
                self.emit_null_check_array_store_at(code, pc, insn_starts);
                self.emit_bounds_check(pc);
                self.load_slot_to_reg(RDX, val_slot);
                self.emit_short_astore_regs();
                self.emit_gpu_input_cache_barrier();
                pc += 1;
            }

            // arraylength — get array length from header (inline)
            0xbe => {
                let arr_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, arr_slot);
                // NPE on null array (JVMS §arraylength). Without this
                // guard `emit_arraylength_regs` does a raw
                // `MOV EAX, [RAX + ARRAY_LENGTH_OFFSET]` which faults on
                // a null receiver. Observed in Tomcat: a JIT-compiled
                // `String.length()` invoked with a null `this` reads its
                // `value` byte[] field (helper safely yields 0), then
                // `arraylength` on the null array SIGSEGV'd the VM
                // instead of throwing NPE. Mirrors the round-9 inline
                // null check that loads/stores already carry; the
                // dedicated `emit_null_check_arraylength` always emits
                // the TEST/JZ since the `_at` dataflow-elision helper
                // parses a load/store bytecode shape that arraylength
                // (no index push) does not match.
                self.emit_null_check_arraylength(code, pc, insn_starts);
                self.emit_arraylength_regs();
                self.push_from_rax();
                pc += 1;
            }
            _ => {
                // `compile_bytecode` routed an opcode here that this family
                // does not lower: a dispatch table bug, refused rather than
                // emitted.
                self.fail("singlepass-codegen/walk-family-misdispatch");
                return WalkStep::Return(false);
            }
        }
        WalkStep::Next(pc)
    }

    /// The `iaload`/`laload`/`baload`/`caload`/`saload` at `pc` as one load
    /// addressing the array and index operands in the registers they already
    /// occupy, the result straight into a free scratch register:
    /// `movsxd r8, [r14 + r12*4 + d]` where the general arm emits
    /// `mov rax, r14 ; mov rcx, r12 ; movsxd rax, [rax+rcx*4+d] ; mov r8, rax`.
    /// Returns whether it lowered the instruction; on `false` nothing was
    /// emitted and the operand stack is untouched.
    ///
    /// The BOUNDS check may stay (round 9 wave 9, arr9): it is emitted as
    /// `cmp <index>d, [<array> + len] ; jae` against the homes
    /// (`emit_bounds_check_in`), and the pad -- which reads RAX/RCX -- is
    /// reached through a per-site cold prologue that moves them there
    /// (`emit_bounds_check_home_prologues`). Refused (general arm) where the
    /// check is the precise reason-11 stub or the unfused
    /// `CRATONVM_JIT_FUSED_BOUNDS_LOAD=0` pair (`array_bounds_check_via_regs_ok`).
    /// The NULL check may stay (round 9
    /// wave 8, arr8): when `array_load_null_check_is_elided` (the very decision
    /// `emit_null_check_array_load_at` takes) does not elide it, it is emitted
    /// as `test <array>, <array> ; jz` against the shared null stub, which
    /// reads no register; inside a protected range under precise frames (the
    /// reason-10 stub) the general arm still runs. The index register is used
    /// as the 64-bit index exactly as RCX was (an int operand is held sign-extended, which is what the general
    /// arm's `mov rcx, r12` relied on too).
    ///
    /// Gated on `operand_fold_enabled()`: `CRATONVM_JIT_NO_OPERAND_FOLD=1`
    /// restores the old bytes here as everywhere else it reaches.
    ///
    /// Round 9 wave 6 (simd6): fix 3 of
    /// `perf-single-pass-scalar-loop-round-trips-every-operand-through-a-scratch-register-20260918.md`.
    fn try_array_load_through_homes(
        &mut self,
        code: &[u8],
        op: u8,
        pc: usize,
        insn_starts: &[bool],
    ) -> bool {
        // Round 9 wave 9 (arr9): a load that keeps its BOUNDS check no longer
        // falls back either -- the compare reads the homes and the per-site
        // cold prologue moves them into RAX/RCX for the pad
        // (`emit_bounds_check_in`). Refused only where that check would be the
        // precise reason-11 stub or the unfused `CMP ECX, R10D` form.
        if !super::operand_stack::operand_fold_enabled()
            || !matches!(op, 0x2e | 0x2f | 0x33 | 0x34 | 0x35)
            || !self.array_bounds_check_via_regs_ok(pc)
        {
            return false;
        }
        // Round 9 wave 8 (arr8): a load that keeps its NULL check (but not its
        // bounds check) no longer falls back to the RAX/RCX arm. The shared
        // null stub reads no register, so the check tests the array where it
        // lives (`emit_null_check_array_load_in`); only the precise reason-10
        // stub inside a protected range still wants the general shape.
        let null_check_elided = self.array_load_null_check_is_elided(code, pc, insn_starts);
        if !null_check_elided && !self.array_load_null_check_via_reg_ok(pc) {
            return false;
        }
        let depth = self.stack.len();
        if depth < 2 {
            return false;
        }
        // Where each operand will be read from: its own register, or the
        // fallback it is loaded into (RAX for the array, RCX for the index).
        let operand_reg = |slot: StackSlot, fallback: u8| -> Option<u8> {
            match slot {
                StackSlot::CalleeSaved(reg) | StackSlot::Scratch(reg, ..) => Some(reg),
                StackSlot::Frame(_) => Some(fallback),
                StackSlot::Xmm(_) => None,
            }
        };
        let (Some(base), Some(index)) = (
            operand_reg(self.stack[depth - 2], RAX),
            operand_reg(self.stack[depth - 1], RCX),
        ) else {
            return false;
        };
        // RSP cannot be a SIB index (and no home or scratch register is RSP);
        // screened before anything is popped.
        if index == RSP || base > 15 || index > 15 {
            return false;
        }
        let index_slot = self.pop_stack();
        let array_slot = self.pop_stack();
        let base_reg = self.slot_to_gpr(array_slot, RAX);
        let index_reg = self.slot_to_gpr(index_slot, RCX);
        debug_assert_eq!((base_reg, index_reg), (base, index));
        if !null_check_elided {
            // `test <array>, <array> ; jz <shared null stub>` -- where the
            // general arm emitted `mov rax, <array> ; mov rcx, <index> ; test
            // rax, rax ; jz`. Nothing is written between here and the load.
            self.emit_null_check_array_load_in(code, pc, base_reg);
        }
        // `cmp <index>d, [<array> + len] ; jae <per-site prologue>`, or
        // nothing when `bounds_safe_pcs` elides it. Same order as the general
        // arm: null check, then bounds check, then the load.
        self.emit_bounds_check_in(pc, base_reg, index_reg);
        // A scratch register no remaining entry names. It may be the one the
        // array or index just vacated: the load reads its address before it
        // writes `dst`.
        let dst = self.free_scratch_reg(None).unwrap_or(RAX);
        let emitted = self.emit_array_load_via_regs(op, dst, base_reg, index_reg);
        debug_assert!(emitted, "opcode and registers were screened above");
        self.push_work_reg(dst);
        true
    }

    /// zt t19/zb2 (zr1br A2b read side, JIT routing): the `aaload` at `pc`
    /// under an ARMED ZGC load barrier, as a call of `jit_aaload`.
    ///
    /// ```text
    ///   <flush scratch>  RAX = array, RCX = index
    ///   <the inline null check and bounds check, exactly as the inline arm>
    ///   ARG0 = vm context, ARG1 = array, ARG2 = index
    ///   CALL jit_aaload                ; reads the element through
    ///                                  ; VmHeap::load_ref_slot_barriered
    ///   <post-invoke sentinel check>   ; RAX = element (machine address or 0)
    /// ```
    ///
    /// # Why a helper and not the inline load
    ///
    /// The inline `MOV RAX, [RAX + RCX*8 + d]` hands compiled code the raw
    /// slot word. In plain-store mode that word is a plain pointer and the
    /// load is correct, but it is still an UNROUTED mutator read (row 29 of
    /// the t18/zb1 inventory): additive marking (`=mark`) never sees it, and
    /// once stores color (A2b) it would hand compiled code a colored word.
    /// `jit_aaload` already reads the element through the heap's barrier seam
    /// (`jit_load_ref_slot`), so routing to it closes the row with no new
    /// barrier code -- the same escape hatch `aastore` takes under the same
    /// gate (`gate_taken` in the `0x53` arm) and the aarch64 backend takes
    /// always (`emit_aaload`).
    ///
    /// # Why compile-time gating is sound
    ///
    /// `zgc_read_barrier_blocks_inline_fields()` reads the process flag
    /// `ZgcRealHeap::set_barrier_color` writes, and the only production
    /// caller arms at heap construction, before any thread runs and before
    /// the first compile; nothing disarms it while the VM runs. So every body
    /// compiled while armed takes this arm, and no body compiled before
    /// arming can exist. A body compiled UNARMED that met an armed heap later
    /// (only a test can arm late) would still read plain words in plain-store
    /// mode -- the mode every production arming uses.
    ///
    /// The guards stay inline (the helper's own report a null array or a bad
    /// index only by the pending-signal channel, and the inline stubs keep the
    /// exception at this bytecode). `jit_aaload` does not allocate and is not
    /// a safepoint, exactly like the `getfield` helper arm, so no oop map is
    /// published. The context slot is reserved by the scan
    /// (`bytecode_compat.rs`, the `0x2e..=0x35` arm); a body whose slot is
    /// missing (an inlined callee) fails closed in `emit_load_vm_context`.
    fn walk_aaload_through_barrier_helper(
        &mut self,
        code: &[u8],
        mut pc: usize,
        insn_starts: &[bool],
    ) -> WalkStep {
        if self.helpers.aaload == 0 {
            // An unwired table (unit-test sentinels only). Refuse rather than
            // fall back to the raw load the armed barrier forbids.
            crate::note_jit_bail_site_at("aaload-zgc-barrier-no-helper", pc, 0x32);
            return WalkStep::Return(false);
        }
        self.flush_scratch_registers();
        let index_slot = self.pop_stack();
        let array_slot = self.pop_stack();
        self.load_slot_to_reg(RAX, array_slot);
        self.load_slot_to_reg(RCX, index_slot);
        self.emit_null_check_array_load_at(code, pc, insn_starts);
        self.emit_bounds_check(pc);
        // Loaded in ARG_REGS order after the flush, so every source is a
        // frame slot, a constant or a callee-saved register (never an
        // `ARG_REGS` member) and no load clobbers a later one's source on
        // either ABI -- the `aastore` fallback's argument.
        self.emit_load_vm_context(ARG_REGS[0]);
        self.load_slot_to_reg(ARG_REGS[1], array_slot);
        self.load_slot_to_reg(ARG_REGS[2], index_slot);
        // (vm_ptr, array_ptr, index) -> element | i64::MIN.
        cratonvm_jit_api::assert_helper_call_shape!("aaload", int_args = 3, returns_value = true);
        self.emit_call_absolute(self.helpers.aaload);
        // `i64::MIN` is never a reference, so the plain compare-and-bail is
        // unambiguous (the sentinel means a pending NPE/AIOOBE or a contained
        // panic, as for `jit_getfield`).
        self.emit_post_invoke_exception_check(b'L');
        self.push_from_rax();
        // T1.1.a -- aaload reads a reference from an Object[].
        self.mark_top_as_oop();
        pc += 1;
        WalkStep::Next(pc)
    }

    /// zt t21/zb4 (stage 2d): `aaload` under an armed COLORED-store ZGC load
    /// barrier, with the barrier's fast path inline instead of the
    /// `jit_aaload` call:
    ///
    /// ```text
    ///   <the inline arm, unchanged>    ; null + bounds checks,
    ///                                  ; MOV RAX, [RAX + RCX*8 + d]
    ///   TEST RAX, RAX ; JNS .done      ; a plain word or null, as is
    ///   MOV R11, mask ; AND RAX, R11   ; the 42-bit offset
    ///   MOV R11, base ; ADD RAX, R11   ; base + offset
    /// .done:
    /// ```
    ///
    /// `base` is `zgc_colored_inline_load_barrier_base()`'s answer, which
    /// also states why no slow path is owed while the good color is pinned at
    /// `Remapped`, and what the color flip must change first. The decode is
    /// `zgc_colored_slot_decode_bytes` (R11 scratch, as the compressed-oops
    /// element decode uses it). No call, so no flush, no context argument and
    /// no sentinel check; the scan's context reservation for an armed `aaload`
    /// (`bytecode_compat.rs`) is left as it is, unused here.
    fn walk_aaload_colored_inline(
        &mut self,
        code: &[u8],
        mut pc: usize,
        insn_starts: &[bool],
        base: u64,
    ) -> WalkStep {
        let index_slot = self.pop_stack();
        let array_slot = self.pop_stack();
        self.load_slot_to_reg(RAX, array_slot);
        self.load_slot_to_reg(RCX, index_slot);
        self.emit_null_check_array_load_at(code, pc, insn_starts);
        self.emit_bounds_check(pc);
        self.emit_ref_aload_regs();
        emit_zgc_colored_slot_decode(&mut self.buf, base);
        if dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] sp zgc-colored-inline-aaload pc={pc} method={}",
                self.method_key
            );
        }
        self.push_from_rax();
        // T1.1.a -- aaload reads a reference from an Object[].
        self.mark_top_as_oop();
        pc += 1;
        WalkStep::Next(pc)
    }

    /// zt t22/zb5 (stage 2e): `aaload` under an armed colored-store ZGC load
    /// barrier with the COLOR FLIP armed -- the inline fast path tests the
    /// loaded word against the heap's compiled bad-mask cell and a bad word
    /// takes a call of `jit_aaload`, which re-reads the slot through
    /// `load_barrier_slot` (mark if marking, self-heal with one CAS):
    ///
    /// ```text
    ///   <flush scratch>  RAX = array, RCX = index
    ///   <the inline null check and bounds check>
    ///   MOV RAX, [RAX + RCX*8 + d]
    ///   <zgc_colored_flip_head_bytes>   ; plain / null -> .done,
    ///                                   ; good colored -> decode, JMP .done
    /// .slow:
    ///   ARG0 = vm context, ARG1 = array, ARG2 = index
    ///   CALL jit_aaload ; <post-invoke sentinel check>
    /// .done:                            ; RAX = element (address or 0)
    /// ```
    ///
    /// The slow branch is the zt t19/zb2 helper arm's call
    /// (`walk_aaload_through_barrier_helper`), so it owes what that arm owes:
    /// the scratch flush BEFORE the branch (both paths must leave the operand
    /// stack in one state, and the call clobbers caller-saved registers), the
    /// context slot the scan reserves (`bytecode_compat.rs` asks
    /// `zgc_ref_element_load_calls_helper`, which answers `true` under the
    /// flip), and a refusal when no helper is wired. `jit_aaload` neither
    /// allocates nor safepoints, so no oop map is published. The operands are
    /// re-loaded from their (flushed) homes for the call, because RAX now
    /// holds the element word.
    fn walk_aaload_colored_flip(
        &mut self,
        code: &[u8],
        mut pc: usize,
        insn_starts: &[bool],
        base: u64,
        cell: u64,
    ) -> WalkStep {
        if self.helpers.aaload == 0 {
            // An unwired table (unit-test sentinels only): never a bad word
            // decoded without its slow path.
            crate::note_jit_bail_site_at("aaload-zgc-color-flip-no-helper", pc, 0x32);
            return WalkStep::Return(false);
        }
        self.flush_scratch_registers();
        let index_slot = self.pop_stack();
        let array_slot = self.pop_stack();
        self.load_slot_to_reg(RAX, array_slot);
        self.load_slot_to_reg(RCX, index_slot);
        self.emit_null_check_array_load_at(code, pc, insn_starts);
        self.emit_bounds_check(pc);
        self.emit_ref_aload_regs();
        let (plain_to_done, good_to_done) = emit_zgc_colored_flip_head(&mut self.buf, cell, base);
        // .slow -- a join of no straight-line predecessor: drop the
        // adjacent-reload mirror before the argument loads.
        self.slot_mirror = None;
        self.emit_load_vm_context(ARG_REGS[0]);
        self.load_slot_to_reg(ARG_REGS[1], array_slot);
        self.load_slot_to_reg(ARG_REGS[2], index_slot);
        // (vm_ptr, array_ptr, index) -> element | i64::MIN.
        cratonvm_jit_api::assert_helper_call_shape!("aaload", int_args = 3, returns_value = true);
        self.emit_call_absolute(self.helpers.aaload);
        self.emit_post_invoke_exception_check(b'L');
        // .done (both patches drop the mirror again: a join).
        self.patch_rel32_to_here(plain_to_done);
        self.patch_rel32_to_here(good_to_done);
        if dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] sp zgc-color-flip-aaload pc={pc} method={}",
                self.method_key
            );
        }
        self.push_from_rax();
        // T1.1.a -- aaload reads a reference from an Object[].
        self.mark_top_as_oop();
        pc += 1;
        WalkStep::Next(pc)
    }
}

/// Is the int the `bastore` at `pc` stores provably `0` or `1`?
///
/// Exactly when the instruction immediately before it is `iconst_0`,
/// `iconst_1`, `bipush 0` or `bipush 1` (a genuine instruction start, per
/// `insn_starts`) and `pc` is not a branch target -- at a merge point the value
/// on the stack may have been pushed on another path. That push is then the
/// top of the stack `bastore` pops as its value, and for 0 and 1 the
/// `boolean[]` narrowing (`& 1`) and the `byte[]` one (low byte) agree, so the
/// runtime element-type test is dead weight. It is javac's `a[i] = true` /
/// `a[i] = false`, i.e. every `boolean[]` store in the sieve-style loops.
///
/// Conservative everywhere else (any computed value, `i2b`, a merge): the
/// caller then emits `emit_bastore_boolean_mask_regs`. Round 9 wave 8 (arr8).
fn bastore_value_is_zero_or_one(
    code: &[u8],
    pc: usize,
    branch_targets: &[bool],
    insn_starts: &[bool],
) -> bool {
    if branch_targets.get(pc).copied().unwrap_or(true) {
        return false;
    }
    let starts_at = |p: usize| insn_starts.get(p).copied().unwrap_or(false);
    // iconst_0 (0x03) / iconst_1 (0x04)
    if pc >= 1 && starts_at(pc - 1) && matches!(code.get(pc - 1).copied(), Some(0x03 | 0x04)) {
        return true;
    }
    // bipush 0 / bipush 1 (0x10 imm8)
    pc >= 2
        && starts_at(pc - 2)
        && code.get(pc - 2).copied() == Some(0x10)
        && matches!(code.get(pc - 1).copied(), Some(0x00 | 0x01))
}

/// `CRATONVM_JIT_SP_AASTORE_DISPLAY_ARM` (JIT r21 wave 2, lane typecheck,
/// TYPECHECK21-5; **default ON (flipped; `=0` kills)**): the single-pass `aastore`
/// proves a subclass-of-component / implementor-of-component value's store
/// legal from the supers displays (`ir_lower::aastore_display_arm_bytes`, the
/// IR tier's `CRATONVM_JIT_IR_AASTORE_DISPLAY_ARM` bytes) instead of calling
/// `jit_aastore_type_check`. Needs `CRATONVM_JIT_TYPECHECK_DISPLAY_ARM`. Read
/// per site.
pub(crate) fn sp_aastore_display_arm_enabled() -> bool {
    crate::ir_lower::typecheck_display_arm_enabled()
        // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_SP_AASTORE_DISPLAY_ARM")
}

/// The display cell the single-pass `aastore` arm bakes, or `None` (switch
/// off, or this VM keeps no displays).
fn sp_aastore_display_cell(helpers: &crate::DirectHelperTable) -> Option<usize> {
    if !sp_aastore_display_arm_enabled() {
        return None;
    }
    crate::ir_lower::typecheck_display_cell(helpers)
}

#[cfg(test)]
mod tests {
    //! Round 9 wave 8 (arr8): the `bastore` value proof that lets the `0x54`
    //! arm skip the `boolean[]` mask.

    use super::bastore_value_is_zero_or_one;

    /// Every pc of `code` starts an instruction except those in `operand_bytes`.
    fn starts(len: usize, operand_bytes: &[usize]) -> Vec<bool> {
        (0..len).map(|p| !operand_bytes.contains(&p)).collect()
    }

    #[test]
    fn a_constant_zero_or_one_right_before_the_store_is_proved() {
        // aload_0; iload_1; iconst_1; bastore
        let code = [0x2a, 0x1b, 0x04, 0x54];
        let st = starts(code.len(), &[]);
        let no_targets = vec![false; code.len()];
        assert!(bastore_value_is_zero_or_one(&code, 3, &no_targets, &st));
        // iconst_0
        let code0 = [0x2a, 0x1b, 0x03, 0x54];
        assert!(bastore_value_is_zero_or_one(&code0, 3, &no_targets, &st));
        // aload_0; iload_1; bipush 1; bastore
        let code_b = [0x2a, 0x1b, 0x10, 0x01, 0x54];
        let st_b = starts(code_b.len(), &[3]);
        let nt_b = vec![false; code_b.len()];
        assert!(bastore_value_is_zero_or_one(&code_b, 4, &nt_b, &st_b));
    }

    #[test]
    fn anything_else_keeps_the_runtime_mask() {
        let no_targets = vec![false; 8];
        // iconst_2 -- the case the page is about.
        let two = [0x2a, 0x1b, 0x05, 0x54];
        assert!(!bastore_value_is_zero_or_one(
            &two,
            3,
            &no_targets,
            &starts(4, &[])
        ));
        // iload_2 -- any computed value.
        let var = [0x2a, 0x1b, 0x1c, 0x54];
        assert!(!bastore_value_is_zero_or_one(
            &var,
            3,
            &no_targets,
            &starts(4, &[])
        ));
        // bipush 2
        let b2 = [0x2a, 0x1b, 0x10, 0x02, 0x54];
        assert!(!bastore_value_is_zero_or_one(
            &b2,
            4,
            &no_targets,
            &starts(5, &[3])
        ));
        // A merge point: another path may reach the store with its own value.
        let one = [0x2a, 0x1b, 0x04, 0x54];
        let mut target = vec![false; 4];
        target[3] = true;
        assert!(!bastore_value_is_zero_or_one(
            &one,
            3,
            &target,
            &starts(4, &[])
        ));
        // An operand byte that merely equals `iconst_1` is not an instruction:
        // `sipush 0x0004` ends in 0x04.
        let sip = [0x2a, 0x1b, 0x11, 0x00, 0x04, 0x54];
        assert!(!bastore_value_is_zero_or_one(
            &sip,
            5,
            &no_targets,
            &starts(6, &[3, 4])
        ));
        // Out-of-range maps answer "not proved".
        assert!(!bastore_value_is_zero_or_one(&one, 3, &[], &starts(4, &[])));
        assert!(!bastore_value_is_zero_or_one(
            &one,
            0,
            &no_targets,
            &starts(4, &[])
        ));
    }

    /// JIT r21 wave 2 (lane typecheck, TYPECHECK21-5): default ON since the r22 A/B.
    #[test]
    fn r21_sp_aastore_display_arm_is_default_on() {
        // R22-AB-FLIP: default ON (with its default-on companion
        // `CRATONVM_JIT_TYPECHECK_DISPLAY_ARM`); `=0` turns it off.
        const KEY: &str = "CRATONVM_JIT_SP_AASTORE_DISPLAY_ARM";
        const DISPLAY: &str = "CRATONVM_JIT_TYPECHECK_DISPLAY_ARM";
        let on = |v: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(&[(KEY, v), (DISPLAY, None)], || {
                super::sp_aastore_display_arm_enabled()
            })
        };
        assert!(on(None));
        assert!(!on(Some("0")));
    }

    /// JIT r21 wave 2 (lane typecheck, TYPECHECK21-5): the single-pass
    /// prefix the arm is emitted with -- `MOV EDX, R10D` (value id) and
    /// `MOV ECX, [R11 + disp32]` (the array's component id) -- in front of
    /// `aastore_display_arm_bytes`, EXECUTED as `fn(value_id, array) -> 1 |
    /// -1` over a fake display table and a fake array header.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn r21_sp_aastore_display_prefix_feeds_the_arm() {
        #[cfg(target_os = "windows")]
        const A: [u8; 2] = [1, 2]; // RCX, RDX
        #[cfg(not(target_os = "windows"))]
        const A: [u8; 2] = [7, 6]; // RDI, RSI
        const BASE: u32 = 0x101;
        const SUB: u32 = 0x102;
        const IFACE: u32 = 0x301;
        let mov = |dst: u8, src: u8| {
            [
                0x48 | (u8::from(src >= 8) << 2) | u8::from(dst >= 8),
                0x89,
                0xC0 | ((src & 7) << 3) | (dst & 7),
            ]
        };
        let rows: [(&[u32], &[u32]); 3] = [(&[1], &[]), (&[1, BASE], &[]), (&[1, BASE, SUB], &[IFACE])];
        let displays: Vec<Box<[u32]>> = rows
            .iter()
            .map(|(c, i)| {
                let mut d = vec![u32::try_from(c.len()).expect("small")];
                d.extend_from_slice(c);
                d.push(u32::try_from(i.len()).expect("small"));
                d.extend_from_slice(i);
                d.push(1);
                d.into_boxed_slice()
            })
            .collect();
        let mut table = vec![0u64; 0x401];
        table[0] = 0x400;
        for ((c, _), d) in rows.iter().zip(&displays) {
            let id = *c.last().expect("non-empty");
            table[1 + usize::try_from(id).expect("small")] = d.as_ptr() as u64; // Cast: address
        }
        let table = table.into_boxed_slice();
        let cell = Box::new(table.as_ptr() as u64); // Cast: address
        let cell_addr = &*cell as *const u64 as usize; // Cast: the cell's address
        let (bytes, decline, hits) = crate::ir_lower::aastore_display_arm_bytes(cell_addr);
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&mov(10, A[0])); // R10 = value id
        body.extend_from_slice(&mov(11, A[1])); // R11 = array
        body.extend_from_slice(&[0x44, 0x89, 0xD2]); // MOV EDX, R10D
        body.extend_from_slice(&[0x41, 0x8B, 0x8B, 0, 0, 0, 0]); // MOV ECX, [R11 + 0]
        let at = body.len();
        body.extend_from_slice(&bytes);
        let hit_at = body.len();
        body.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, 1 ; RET
        let decline_at = body.len();
        body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3]);
        let mut patch = |field: usize, to: usize| {
            let operand = at + field;
            let rel =
                i32::try_from(to).expect("small") - i32::try_from(operand + 4).expect("small");
            body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
        };
        for d in decline {
            patch(d, decline_at);
        }
        for h in hits {
            patch(h, hit_at);
        }
        let mut buf = crate::ExecutableBuffer::new(1024).expect("executable buffer");
        buf.emit(&body);
        assert!(!buf.overflowed());
        buf.finalize();
        // SAFETY: a complete `extern "C" fn(u64, u64) -> i64` reading only the
        // fixtures kept alive below and writing caller-saved registers.
        let f = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> i64>(buf.as_ptr())
        };
        let array = |component: u32| Box::new([u64::from(component), 0, 0, 0]);
        let (base_arr, iface_arr, sub_arr) = (array(BASE), array(IFACE), array(SUB));
        let ask = |v: u32, a: &[u64; 4]| f(u64::from(v), a.as_ptr() as u64);
        assert_eq!(ask(SUB, &base_arr), 1, "Sub into Base[]");
        assert_eq!(ask(SUB, &iface_arr), 1, "Sub into IFACE[]");
        assert_eq!(ask(BASE, &sub_arr), -1, "Base into Sub[] declines");
        assert_eq!(ask(BASE, &iface_arr), -1, "Base into IFACE[] declines");
        drop((cell, displays, table));
    }
}

#[cfg(test)]
mod zt19zb2_tests {
    //! zt t19/zb2: the single-pass `aaload` arm under an armed ZGC load
    //! barrier, driven directly -- no process flag is flipped, so the other
    //! tests of this binary keep their unarmed emission.

    use super::*;

    /// A fresh compiler whose operand stack holds an array and an index in
    /// frame slots, with a reserved VM-context slot and `aaload_helper` wired.
    fn compiler_with_aaload_operands(aaload_helper: usize) -> Compiler {
        let mut c = crate::x64::emit::tests::test_compiler();
        c.helpers.aaload = aaload_helper;
        c.heap_local_offset = 8;
        for off in [16, 24] {
            c.stack.push(StackSlot::Frame(off));
            c.stack_oop_marks.push(false);
        }
        c
    }

    /// An unwired helper table refuses the site: never the raw load the
    /// armed barrier forbids, and nothing is popped.
    #[test]
    fn zt19zb2_an_unwired_aaload_helper_refuses_the_armed_site() {
        let mut c = compiler_with_aaload_operands(0);
        let step = c.walk_aaload_through_barrier_helper(&[0x32], 0, &[true]);
        assert!(matches!(step, WalkStep::Return(false)));
        assert_eq!(c.stack.len(), 2, "nothing was popped");
    }

    /// The armed arm calls `jit_aaload` and emits no raw element load; the
    /// element replaces both operands and is marked as an oop.
    #[test]
    fn zt19zb2_the_armed_aaload_calls_the_helper_instead_of_the_inline_load() {
        const HELPER: usize = 0x7FFF_0000_1000;
        let mut c = compiler_with_aaload_operands(HELPER);
        let start = c.buf.pos();
        let calls_before = c.helper_call_patches.len();
        let step = c.walk_aaload_through_barrier_helper(&[0x32], 0, &[true]);
        let code = c.buf.as_slice()[start..].to_vec();
        assert!(
            matches!(step, WalkStep::Next(1)),
            "the walk continues after the aaload"
        );
        assert_eq!(c.stack.len(), 1, "the element replaces both operands");
        assert_eq!(c.stack_oop_marks.last(), Some(&true), "and is an oop");
        let addr = (HELPER as u64).to_le_bytes();
        let called = c.helper_call_patches.len() > calls_before
            || code.windows(addr.len()).any(|w| w == &addr[..]);
        assert!(called, "a call of the helper: {code:02x?}");
        // MOV RAX, [RAX + RCX*8 + disp8], the inline arm's load.
        let disp = u8::try_from(ARRAY_DATA_OFFSET).expect("a disp8 element base");
        let inline = [0x48u8, 0x8B, 0x44, 0xC8, disp];
        assert!(
            !code.windows(inline.len()).any(|w| w == &inline[..]),
            "no raw element load: {code:02x?}"
        );
    }
}

#[cfg(test)]
mod zt21zb4_tests {
    //! zt t21/zb4 (stage 2d): the inline colored-slot decode after a compiled
    //! `aaload`, driven directly with an explicit base -- no process flag is
    //! flipped and no base is published, so the other tests of this binary
    //! keep their unarmed emission.

    use super::*;

    const BASE: u64 = 0x0000_7F00_0000_0000;

    /// A fresh compiler whose operand stack holds an array and an index in
    /// frame slots (no helpers wired: the colored arm calls none).
    fn compiler_with_aaload_operands() -> Compiler {
        let mut c = crate::x64::emit::tests::test_compiler();
        for off in [16, 24] {
            c.stack.push(StackSlot::Frame(off));
            c.stack_oop_marks.push(false);
        }
        c
    }

    /// The emitted sequence, byte for byte (the report quotes it).
    #[test]
    fn zt21zb4_the_decode_bytes_are_the_documented_sequence() {
        let bytes = zgc_colored_slot_decode_bytes(BASE);
        let mut want = vec![0x48, 0x85, 0xC0, 0x79, 0x1A, 0x49, 0xBB];
        want.extend_from_slice(&((1u64 << 42) - 1).to_le_bytes());
        want.extend_from_slice(&[0x4C, 0x21, 0xD8, 0x49, 0xBB]);
        want.extend_from_slice(&BASE.to_le_bytes());
        want.extend_from_slice(&[0x4C, 0x01, 0xD8]);
        assert_eq!(bytes.to_vec(), want);
        assert_eq!(bytes.len(), ZGC_COLORED_SLOT_DECODE_LEN);
        // The JNS skips exactly the four instructions after it.
        assert_eq!(usize::from(bytes[4]), ZGC_COLORED_SLOT_DECODE_LEN - 5);
    }

    /// The colored arm is the unarmed inline arm with the decode spliced in
    /// right after the element load, and calls nothing; the unarmed arm itself
    /// carries no decode (its bytes are what the default path emits).
    #[test]
    fn zt21zb4_the_colored_aaload_is_the_inline_arm_plus_the_decode() {
        // zt t22/x18: the answer read below must hold through the walk.
        let _flip = crate::x64::licm::zgc_read_gate_flip_test_lock();
        if zgc_read_barrier_blocks_inline_fields() {
            // Another test of this binary armed the process flag; the unarmed
            // twin below would take the routed arm. Nothing to compare.
            return;
        }
        let mut plain = compiler_with_aaload_operands();
        let p0 = plain.buf.pos();
        let step = plain.walk_array(&[0x32], 1, 0x32, 0, &mut false, &[false], &[true]);
        assert!(matches!(step, WalkStep::Next(1)));
        let plain_code = plain.buf.as_slice()[p0..].to_vec();

        let mut colored = compiler_with_aaload_operands();
        let c0 = colored.buf.pos();
        let calls_before = colored.helper_call_patches.len();
        let step = colored.walk_aaload_colored_inline(&[0x32], 0, &[true], BASE);
        assert!(
            matches!(step, WalkStep::Next(1)),
            "the walk continues after the aaload"
        );
        assert_eq!(colored.stack.len(), 1, "the element replaces both operands");
        assert_eq!(colored.stack_oop_marks.last(), Some(&true), "and is an oop");
        assert_eq!(
            colored.helper_call_patches.len(),
            calls_before,
            "no helper call"
        );
        let colored_code = colored.buf.as_slice()[c0..].to_vec();

        // MOV RAX, [RAX + RCX*8 + disp8], the inline arm's load.
        let disp = u8::try_from(ARRAY_DATA_OFFSET).expect("a disp8 element base");
        let load = [0x48u8, 0x8B, 0x44, 0xC8, disp];
        let at = plain_code
            .windows(load.len())
            .position(|w| w == &load[..])
            .expect("the unarmed arm loads the element inline")
            + load.len();
        let decode = zgc_colored_slot_decode_bytes(BASE);
        assert!(
            !plain_code.windows(decode.len()).any(|w| w == &decode[..]),
            "the unarmed arm emits no decode"
        );
        let mut want = plain_code[..at].to_vec();
        want.extend_from_slice(&decode);
        want.extend_from_slice(&plain_code[at..]);
        assert_eq!(
            colored_code, want,
            "colored = inline arm + decode after the load"
        );
    }

    /// Unarmed (every default run), the predicate answers `None` and no
    /// element load calls the helper: the default path is the inline arm.
    #[test]
    fn zt21zb4_an_unarmed_process_keeps_the_inline_arm() {
        // zt t22/x18: as above.
        let _flip = crate::x64::licm::zgc_read_gate_flip_test_lock();
        if zgc_read_barrier_blocks_inline_fields() {
            return;
        }
        assert_eq!(zgc_colored_inline_load_barrier_base(), None);
        assert!(!zgc_ref_element_load_calls_helper());
    }

    /// Both optimizing-tier halves ask the ONE predicate (the allocator's
    /// call set and the lowering's helper arm), and the element emitter
    /// appends the shared decode. Needles are assembled so this file does not
    /// match them.
    #[test]
    fn zt21zb4_the_optimizing_tier_asks_one_predicate() {
        let pred = concat!("crate::x64::zgc_ref_element_load_calls_", "helper()");
        let lower = include_str!("../ir_lower.rs");
        let alloc = include_str!("../regalloc.rs");
        assert!(lower.contains(pred), "ir_lower's ArrayLoad(Ref) arm");
        assert!(alloc.contains(pred), "regalloc::ir_op_is_call");
        let emit = concat!("emit_zgc_colored_slot_", "decode(&mut self.buf, base)");
        assert!(
            lower.contains(emit),
            "emit_gpr_array_elem_load's colored arm"
        );
    }

    /// The decode EXECUTED on a word buffer: `extern "C" fn(word) -> word'`.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn zt21zb4_the_decode_runs_and_agrees_with_the_gc_readers_decode() {
        use cratonvm_types::narrow_oop::{decode_colored_slot_word_with, COLORED_SLOT_TAG};
        let mut buf = crate::ExecutableBuffer::new(256).expect("an executable buffer");
        // MOV RAX, ARG0 (RCX on Win64, RDI on SysV; both below R8).
        let arg0 = ARG_REGS[0];
        buf.emit(&[0x48, 0x89, 0xC0 | ((arg0 & 7) << 3)]);
        emit_zgc_colored_slot_decode(&mut buf, BASE);
        buf.emit(&[0xC3]); // RET
        buf.finalize();
        // SAFETY: a finalized `extern "C" fn(u64) -> u64` that reads its
        // argument and writes RAX, R11 and the flags (all caller-saved).
        let f =
            unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64) -> u64>(buf.as_ptr()) };
        let remapped = 1u64 << 44;
        let marked0 = 1u64 << 42;
        let max_off = (1u64 << 42) - 1;
        let words: [(u64, u64); 7] = [
            // null stays null
            (0, 0),
            // a plain machine pointer passes through
            (0x0000_7FFF_1234_5678, 0x0000_7FFF_1234_5678),
            (0x0000_0000_0040_0010, 0x0000_0000_0040_0010),
            // good colored words decode to base + offset
            (COLORED_SLOT_TAG | remapped | 0x1238, BASE + 0x1238),
            // an object at arena offset 0 is `base`, never null
            (COLORED_SLOT_TAG | remapped, BASE),
            (COLORED_SLOT_TAG | remapped | max_off, BASE + max_off),
            // the pinned decode ignores the color: a Marked0 word decodes the
            // same -- why the color flip must withdraw this sequence
            (COLORED_SLOT_TAG | marked0 | 0x80, BASE + 0x80),
        ];
        for (word, want) in words {
            assert_eq!(f(word), want, "word {word:#018x}");
            assert_eq!(
                f(word),
                decode_colored_slot_word_with(BASE, word),
                "the GC readers' decode agrees on {word:#018x}"
            );
        }
    }
}

#[cfg(test)]
mod zt22zb5_tests {
    //! zt t22/zb5 (stage 2e): the color flip's compiled element load, driven
    //! directly with an explicit cell address and base -- no process flag is
    //! flipped and no cell is published, so the other tests of this binary
    //! keep their emission.

    use super::*;

    const BASE: u64 = 0x0000_7F00_0000_0000;
    const CELL: u64 = 0x0000_7F11_2233_4440;
    const HELPER: usize = 0x7FFF_0000_1000;

    /// A fresh compiler whose operand stack holds an array and an index in
    /// frame slots, with a reserved VM-context slot and `aaload` wired.
    fn compiler_with_aaload_operands(aaload_helper: usize) -> Compiler {
        let mut c = crate::x64::emit::tests::test_compiler();
        c.helpers.aaload = aaload_helper;
        c.heap_local_offset = 8;
        for off in [16, 24] {
            c.stack.push(StackSlot::Frame(off));
            c.stack_oop_marks.push(false);
        }
        c
    }

    fn le32(bytes: &[u8], at: usize) -> i32 {
        i32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    }

    /// The head, byte for byte (the report quotes it), its two patch slots,
    /// and the JNZ that skips exactly the decode and the closing JMP.
    #[test]
    fn zt22zb5_the_flip_head_bytes_are_the_documented_sequence() {
        let bytes = zgc_colored_flip_head_bytes(CELL, BASE);
        let mut want = vec![0x48, 0x85, 0xC0, 0x0F, 0x89, 0, 0, 0, 0, 0x49, 0xBB];
        want.extend_from_slice(&CELL.to_le_bytes());
        want.extend_from_slice(&[0x49, 0x85, 0x03, 0x75, 0x1F, 0x49, 0xBB]);
        want.extend_from_slice(&((1u64 << 42) - 1).to_le_bytes());
        want.extend_from_slice(&[0x4C, 0x21, 0xD8, 0x49, 0xBB]);
        want.extend_from_slice(&BASE.to_le_bytes());
        want.extend_from_slice(&[0x4C, 0x01, 0xD8, 0xE9, 0, 0, 0, 0]);
        assert_eq!(bytes.to_vec(), want);
        assert_eq!(bytes.len(), ZGC_COLORED_FLIP_HEAD_LEN);
        // The patch slots are the two rel32 placeholders.
        assert_eq!(
            &bytes[ZGC_COLORED_FLIP_JNS_PATCH - 2..ZGC_COLORED_FLIP_JNS_PATCH],
            &[0x0F, 0x89]
        );
        assert_eq!(bytes[ZGC_COLORED_FLIP_JMP_PATCH - 1], 0xE9);
        assert_eq!(ZGC_COLORED_FLIP_JMP_PATCH + 4, ZGC_COLORED_FLIP_HEAD_LEN);
        // JNZ rel8 at 22..24 lands on .slow, the first byte after the head.
        assert_eq!(24 + usize::from(bytes[23]), ZGC_COLORED_FLIP_HEAD_LEN);
        // The good-word arm is stage 2d's decode minus its sign test.
        let decode = zgc_colored_slot_decode_bytes(BASE);
        assert_eq!(&bytes[24..50], &decode[5..]);
    }

    /// The flip arm: the unarmed arm's bytes up to the element load, then the
    /// head, then a call of `jit_aaload`, with both rel32 exits patched to
    /// the end of the site (`.done`); the element is one oop on the stack.
    #[test]
    fn zt22zb5_the_flip_aaload_is_the_inline_load_the_head_and_a_helper_call() {
        let _flip = crate::x64::licm::zgc_read_gate_flip_test_lock();
        if zgc_read_barrier_blocks_inline_fields() {
            // Another test of this binary armed the process flag; the unarmed
            // twin below would take the routed arm. Nothing to compare.
            return;
        }
        let mut plain = compiler_with_aaload_operands(HELPER);
        let p0 = plain.buf.pos();
        let step = plain.walk_array(&[0x32], 1, 0x32, 0, &mut false, &[false], &[true]);
        assert!(matches!(step, WalkStep::Next(1)));
        let plain_code = plain.buf.as_slice()[p0..].to_vec();

        let mut flip = compiler_with_aaload_operands(HELPER);
        let f0 = flip.buf.pos();
        let calls_before = flip.helper_call_patches.len();
        let step = flip.walk_aaload_colored_flip(&[0x32], 0, &[true], BASE, CELL);
        assert!(
            matches!(step, WalkStep::Next(1)),
            "the walk continues after the aaload"
        );
        assert_eq!(flip.stack.len(), 1, "the element replaces both operands");
        assert_eq!(flip.stack_oop_marks.last(), Some(&true), "and is an oop");
        let code = flip.buf.as_slice()[f0..].to_vec();
        let addr = (HELPER as u64).to_le_bytes();
        let called = flip.helper_call_patches.len() > calls_before
            || code.windows(addr.len()).any(|w| w == &addr[..]);
        assert!(called, "the slow branch calls the helper: {code:02x?}");

        // MOV RAX, [RAX + RCX*8 + disp8], the inline arm's load.
        let disp = u8::try_from(ARRAY_DATA_OFFSET).expect("a disp8 element base");
        let load = [0x48u8, 0x8B, 0x44, 0xC8, disp];
        let at = code
            .windows(load.len())
            .position(|w| w == &load[..])
            .expect("the flip arm loads the element inline")
            + load.len();
        // The pinned 2d decode is not emitted.
        let decode = zgc_colored_slot_decode_bytes(BASE);
        assert!(
            !code.windows(decode.len()).any(|w| w == &decode[..]),
            "no pinned decode under the flip"
        );
        // The head right after the load; both exits patched to the end.
        let head = zgc_colored_flip_head_bytes(CELL, BASE);
        assert_eq!(&code[at..at + 5], &head[..5], "TEST + JNS opcode");
        assert_eq!(
            &code[at + 9..at + 51],
            &head[9..51],
            "cell, bad test, decode"
        );
        let end = code.len();
        let jns = at + ZGC_COLORED_FLIP_JNS_PATCH;
        let jmp = at + ZGC_COLORED_FLIP_JMP_PATCH;
        // `.done` is where the element is pushed: depending on the operand
        // stack's register state, that push is nothing (a register) or one
        // frame-slot store (zt t24: 4 bytes after `.done` on the host run).
        let done = jns + 4 + le32(&code, jns) as usize;
        assert_eq!(jmp + 4 + le32(&code, jmp) as usize, done, "JMP -> .done");
        assert!(
            done <= end && end - done <= 8,
            "JNS -> .done, followed by the element's push only: done={done} end={end}"
        );
        // Up to the load, the flip arm is the unarmed arm (both operands sat
        // in frame slots, so the flush emitted nothing).
        let plain_at = plain_code
            .windows(load.len())
            .position(|w| w == &load[..])
            .expect("the unarmed arm loads the element inline");
        assert_eq!(&code[..at - load.len()], &plain_code[..plain_at]);
    }

    /// An unwired helper table refuses the flip site (never a bad word
    /// decoded without its slow path), and nothing is popped.
    #[test]
    fn zt22zb5_an_unwired_aaload_helper_refuses_the_flip_site() {
        let mut c = compiler_with_aaload_operands(0);
        let step = c.walk_aaload_colored_flip(&[0x32], 0, &[true], BASE, CELL);
        assert!(matches!(step, WalkStep::Return(false)));
        assert_eq!(c.stack.len(), 2, "nothing was popped");
    }

    /// Unarmed (every default run) no cell is offered and no element load
    /// calls the helper.
    #[test]
    fn zt22zb5_an_unarmed_process_offers_no_flip_cell() {
        // x18's lock: licm's coexistence test flips the read gate between
        // this read and the asserts otherwise.
        let _flip = crate::x64::licm::zgc_read_gate_flip_test_lock();
        if zgc_read_barrier_blocks_inline_fields() {
            return;
        }
        assert_eq!(zgc_colored_flip_bad_mask_cell(), None);
        assert!(!zgc_ref_element_load_calls_helper());
    }

    /// The optimizing tier: its `ArrayLoad(Ref)` helper arm dispatches to the
    /// flip lowering, which loads the raw word through the shared element
    /// emitter and appends the shared head; the predicate the allocator and
    /// the scan ask covers the flip. Needles are assembled so this file does
    /// not match them.
    #[test]
    fn zt22zb5_the_optimizing_tier_emits_the_same_head() {
        let lower = include_str!("../ir_lower.rs");
        for needle in [
            concat!(
                "self.lower_ref_array_load_colored_",
                "flip(id, node, slot, bci, base, cell)"
            ),
            concat!("self.emit_gpr_array_elem_load_", "with(MemKind::Ref, true)"),
            concat!(
                "crate::x64::emit_zgc_colored_flip_",
                "head(&mut self.buf, cell, base)"
            ),
        ] {
            assert!(lower.contains(needle), "ir_lower.rs lacks {needle}");
        }
        let licm = include_str!("licm.rs");
        assert!(
            licm.contains(concat!(
                "|| zgc_colored_flip_bad_mask_",
                "cell().is_some())"
            )),
            "zgc_ref_element_load_calls_helper covers the flip"
        );
    }

    /// The head EXECUTED: `extern "C" fn(word) -> word'`, with the slow
    /// branch replaced by `MOV RAX, SLOW` so a bad word is visible. Agrees
    /// with the collector's executable statement of the same fast path,
    /// `cratonvm_gc::zgc::barrier::inline_barrier_fast`, for every color
    /// under the bad mask of each good color -- and the cell is re-read on
    /// every call: the mask changes between calls with no recompile.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn zt22zb5_the_flip_head_runs_and_agrees_with_the_barriers_fast_path() {
        use cratonvm_gc::zgc::barrier::{inline_barrier_fast, ZInlineFast};
        use cratonvm_gc::zgc::vaddr::{ZColor, Z_METADATA_MASK};
        use std::sync::atomic::{AtomicU64, Ordering};
        const SLOW: u64 = 0x5107_5107_5107_5107;
        // A frame local, not a `static` (JIT r25 wave 8, W8-2): it outlives
        // every call of the head below, which is all the baked address needs.
        let cell_word = AtomicU64::new(0);
        let cell = &cell_word as *const AtomicU64 as u64;

        let mut buf = crate::ExecutableBuffer::new(256).expect("an executable buffer");
        // MOV RAX, ARG0 (RCX on Win64, RDI on SysV; both below R8).
        let arg0 = ARG_REGS[0];
        buf.emit(&[0x48, 0x89, 0xC0 | ((arg0 & 7) << 3)]);
        let (jns, jmp) = emit_zgc_colored_flip_head(&mut buf, cell, BASE);
        // .slow: MOV RAX, imm64 SLOW
        buf.emit(&[0x48, 0xB8]);
        buf.emit(&SLOW.to_le_bytes());
        // .done
        crate::runtime_lowering::patch_rel32_to_here(&mut buf, jns);
        crate::runtime_lowering::patch_rel32_to_here(&mut buf, jmp);
        buf.emit(&[0xC3]); // RET
        buf.finalize();
        // SAFETY: a finalized `extern "C" fn(u64) -> u64` that reads its
        // argument and `cell_word`, and writes RAX, R11 and the flags (all
        // caller-saved).
        let f =
            unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64) -> u64>(buf.as_ptr()) };
        let tag = 1u64 << 63;
        let max_off = (1u64 << 42) - 1;
        for good in [ZColor::Remapped, ZColor::Marked0, ZColor::Marked1] {
            let bad = good.bit() ^ Z_METADATA_MASK;
            cell_word.store(bad, Ordering::Relaxed);
            let mut words = vec![0u64, 0x0000_7FFF_1234_5678, 0x0000_0000_0040_0010];
            for c in ZColor::ALL {
                for off in [0u64, 0x1238, max_off] {
                    words.push(tag | c.bit() | off);
                }
            }
            for word in words {
                let want = match inline_barrier_fast(word, bad, BASE) {
                    ZInlineFast::Done(v) => v,
                    ZInlineFast::Slow(_) => SLOW,
                };
                assert_eq!(f(word), want, "good {good:?}, word {word:#018x}");
            }
            // The good color itself passes and decodes; offset 0 is `base`.
            assert_eq!(f(tag | good.bit() | 0x40), BASE + 0x40);
            assert_eq!(f(tag | good.bit()), BASE);
        }
    }
}
