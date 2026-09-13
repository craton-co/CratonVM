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
        _code_len: usize,
        op: u8,
        mut pc: usize,
        _dead: &mut bool,
        _branch_targets: &[bool],
    ) -> WalkStep {
        match op {

            // iaload — load int from int[] array (inline)
            0x2e => {
                let index_slot = self.pop_stack();
                let array_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-9 HIGH fix: NPE on null array (JVMS §iaload).
                self.emit_null_check_array_load_at(code, pc);
                self.emit_bounds_check(pc);
                self.emit_int_aload_regs();
                self.push_from_rax();
                pc += 1;
            }

            // aaload — load reference from Object[] array (inline, 8 bytes/element)
            0x32 => {
                let index_slot = self.pop_stack();
                let array_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-9 HIGH fix: NPE on null array (JVMS §aaload).
                self.emit_null_check_array_load_at(code, pc);
                self.emit_bounds_check(pc);
                self.emit_ref_aload_regs();
                self.push_from_rax();
                // T1.1.a — aaload reads a reference from an Object[].
                self.mark_top_as_oop();
                pc += 1;
            }

            // laload — load long from long[] array (inline, 8 bytes/element)
            0x2f => {
                let index_slot = self.pop_stack();
                let array_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-9 HIGH fix: NPE on null array (JVMS §laload).
                self.emit_null_check_array_load_at(code, pc);
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
                self.emit_null_check_array_load_at(code, pc);
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
                self.emit_null_check_array_load_at(code, pc);
                self.emit_bounds_check(pc);
                // Double is 8 bytes, same as long; value is stored as bit pattern
                self.emit_long_aload_regs();
                self.push_from_rax_as_xmm0();
                pc += 1;
            }

            // baload — load byte/boolean from byte[]/boolean[] array (inline)
            0x33 => {
                let index_slot = self.pop_stack();
                let array_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-9 HIGH fix: NPE on null array (JVMS §baload).
                self.emit_null_check_array_load_at(code, pc);
                self.emit_bounds_check(pc);
                self.emit_byte_aload_regs();
                self.push_from_rax();
                pc += 1;
            }

            // caload — load char from char[] array (inline, 2 bytes/element, zero-extend)
            0x34 => {
                let index_slot = self.pop_stack();
                let array_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-9 HIGH fix: NPE on null array (JVMS §caload).
                self.emit_null_check_array_load_at(code, pc);
                self.emit_bounds_check(pc);
                self.emit_char_aload_regs();
                self.push_from_rax();
                pc += 1;
            }

            // saload — load short from short[] array (inline, 2 bytes/element, sign-extend)
            0x35 => {
                let index_slot = self.pop_stack();
                let array_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-9 HIGH fix: NPE on null array (JVMS §saload).
                self.emit_null_check_array_load_at(code, pc);
                self.emit_bounds_check(pc);
                self.emit_short_aload_regs();
                self.push_from_rax();
                pc += 1;
            }

            // iastore — store int to int[] array (inline)
            0x4f => {
                let val_slot = self.pop_stack();
                let index_slot = self.pop_stack();
                let array_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-8 CRIT fix: NPE on null array (JVMS §iastore).
                self.emit_null_check_array_store_at(code, pc);
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
            // docs/known-issues/jdk-only/W7-38-jit-aastore-never-called-its-own-check.md.
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
                let gate_taken = barrier_armed && !no_aastore_barrier_gate();
                if barrier_armed && !gate_taken {
                    AASTORE_ZGC_GATE_SUPPRESSED
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
                self.emit_null_check_array_store_at(code, pc);
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
                    if self.helpers.aastore == 0 {
                        // An unwired helper table (only the unit-test
                        // sentinel tables leave this zero). Refuse the
                        // method rather than emit a call to address 0 --
                        // and, more to the point, rather than fall through
                        // to the inline sequence, which with the barrier
                        // armed is precisely the defect this gate exists to
                        // avoid. Failing closed is the only correct answer.
                        crate::note_jit_bail_site_at("aastore-zgc-barrier-no-helper", pc, 0x53);
                        return WalkStep::Return( false);
                    }
                    AASTORE_ZGC_GATE_FALLBACKS
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // Args: (vm_ptr, array_ptr, index, val). Loaded in
                    // ARG_REGS order after `flush_scratch_registers`, so
                    // every source is a frame slot (or a callee-saved
                    // register, which is never an `ARG_REGS` member) and no
                    // load can clobber a later one's source on either ABI.
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                    self.load_slot_to_reg(ARG_REGS[1], array_slot);
                    self.load_slot_to_reg(ARG_REGS[2], index_slot);
                    self.load_slot_to_reg(ARG_REGS[3], val_slot);
                    // `jit_aastore` builds an ArrayStoreException on its
                    // refusal path, so it allocates: spill and publish an
                    // oop map exactly as the inline path does around
                    // `aastore_type_check`.
                    self.emit_pre_safepoint_spill();
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
                // costing a test and a not-taken jump. Everything else pays
                // one call, which is the price of the JVMS rule; the arm
                // already makes one (SATB) to two (card mark) helper calls.
                self.load_slot_to_reg(RDX, val_slot);
                self.buf.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
                self.buf.emit(&[0x0F, 0x84]); // JZ rel32 -> past the call
                let ase_skip_patch = self.buf.pos();
                self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                // Args: (vm_ptr, array_ptr, value_ptr).
                self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                self.load_slot_to_reg(ARG_REGS[1], array_slot);
                self.load_slot_to_reg(ARG_REGS[2], val_slot);
                // The refusal path allocates (it builds the throwable), so
                // spill and publish an oop map exactly as `checkcast` does.
                self.emit_pre_safepoint_spill();
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
                // The call clobbers the scratch registers; re-establish
                // RAX=array / RCX=index for the SATB load below.
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                // Round-7 fix (CRIT, UAF in JIT): SATB pre-write barrier.
                // Inline-load the OLD reference at the slot and pipe it
                // through `jit_satb_pre_write_barrier(vm_ptr, old_ref)`
                // BEFORE the inline store overwrites it. The helper
                // short-circuits via a single Acquire load when no
                // concurrent mark cycle is in flight (`SatbQueue::
                // is_active() == false`), so the steady-state cost is
                // just an inline load + a not-taken-branch call. Without
                // this, a still-live reference overwritten by JIT code
                // during concurrent marking would be silently dropped by
                // the marker → use-after-free on the next mixed
                // evacuation (audit: round7-gc.md §1).
                //
                // Save RAX (array) / RCX (index) into argument registers
                // first since `emit_ref_aload_regs` clobbers RAX with
                // the loaded value.
                self.emit_ref_aload_regs(); // RAX = OLD ref value
                self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                self.emit_mov_reg_reg(ARG_REGS[1], RAX);
                self.emit_call_absolute(self.helpers.satb_pre_write_barrier);
                // Reload array / index / new value (helper call may have
                // clobbered scratch registers including RAX, RCX, RDX).
                self.load_slot_to_reg(RAX, array_slot);
                self.load_slot_to_reg(RCX, index_slot);
                self.load_slot_to_reg(RDX, val_slot);
                // Inline store: MOV QWORD [RAX + RCX*8 + HEADER_SIZE], RDX
                self.emit_ref_astore_regs();
                // Post-store publication. Generational GC exposes a stable
                // atomic card map, so RAX=array/RDX=value can mark it
                // inline without a helper transition. G1/ZGC retain their
                // collector-specific helper.
                if self.inline_card_mark_available() {
                    self.emit_inline_card_mark_regs(RAX, RDX);
                } else {
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                    self.load_slot_to_reg(ARG_REGS[1], array_slot);
                    self.load_slot_to_reg(ARG_REGS[2], val_slot);
                    self.emit_call_absolute(self.helpers.write_barrier);
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
                self.emit_null_check_array_store_at(code, pc);
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
                self.emit_null_check_array_store_at(code, pc);
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
                self.emit_null_check_array_store_at(code, pc);
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
                self.emit_null_check_array_store_at(code, pc);
                self.emit_bounds_check(pc);
                self.load_slot_to_reg(RDX, val_slot);
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
                self.emit_null_check_array_store_at(code, pc);
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
                self.emit_null_check_array_store_at(code, pc);
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
                self.emit_null_check_arraylength(code, pc);
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
}
