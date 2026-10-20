// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Field access: `getstatic`, `putstatic`, `getfield` and `putfield` in the single-pass backend's bytecode walk.
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
    ///
    /// # Volatile `putfield`: the StoreLoad fence
    ///
    /// A `putfield` whose pc is in `volatile_field_pcs` is followed by
    /// the StoreLoad barrier ([`Self::emit_volatile_store_fence`]), whichever
    /// heap arm lowered it (gated or compact inline, the checked helper); the
    /// scalar-replaced arm owes none. x86-64 TSO already makes a plain store a
    /// release and a plain load an acquire; what a volatile write still owes
    /// the JMM is StoreLoad, so that a later volatile read cannot be satisfied
    /// before this store is globally visible (a Dekker handshake in which both
    /// threads read the other's flag as `false`). It is the instance twin of
    /// the post-`putstatic` fence in the `0xb3` arm below.
    ///
    /// Emitted HERE, after the arm returns, rather than inside each arm: every
    /// arm's fast and slow paths rejoin inline before the arm returns `Next`,
    /// so one fence at that join covers all of them, and an arm added later
    /// cannot forget it. A `Return` (the compile is abandoned) emits nothing.
    ///
    /// A volatile `getfield` needs no instruction on x86-64. What it needs is
    /// that no transform treats it as invariant: the single-pass backend has no
    /// field-load hoist (`loop_analysis::find_invariant_loads` was deleted in
    /// round 9 wave 3; the `aaload` / `arraylength` hoists never read a field,
    /// and a `BoundSource::Field` loop limit has no pre-header guard encoding),
    /// and the optimizing tier refuses the method outright
    /// (`ir::IrBuilder::set_volatile_field_pcs`).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn walk_field(
        &mut self,
        code: &[u8],
        code_len: usize,
        op: u8,
        pc: usize,
        dead: &mut bool,
        branch_targets: &[bool],
        insn_starts: &[bool],
    ) -> WalkStep {
        // A scalar-replaced object never escapes this frame, so no other
        // thread can synchronize with its volatile field and the store owes no
        // barrier (C2's escape analysis drops the same membar). The arm below
        // takes the frame-slot path exactly when `scalar_field_ops` names `pc`.
        let volatile_store = op == 0xb5
            && self.volatile_field_pcs.contains(&pc)
            && !self.scalar_field_ops.contains_key(&pc);
        let step = self.walk_field_arm(code, code_len, op, pc, dead, branch_targets, insn_starts);
        if volatile_store && matches!(step, WalkStep::Next(_)) {
            self.emit_volatile_store_fence();
        }
        step
    }

    /// The StoreLoad barrier after a volatile store: `LOCK ADD DWORD [RSP], 0`
    /// (`F0 83 04 24 00`), not `MFENCE`.
    ///
    /// On x86-64 TSO a volatile store owes the JMM only StoreLoad, and any
    /// `LOCK`-prefixed read-modify-write provides it. `MFENCE` additionally
    /// orders non-temporal / write-combining accesses this code never makes and
    /// costs tens of cycles more on Intel cores; HotSpot has emitted the locked
    /// add for years. `[RSP]` is always mapped, is this thread's own line (so
    /// the lock never contends), and adding 0 leaves the word unchanged for
    /// anything that reads it. The volatile LOAD fence behind
    /// `CRATONVM_JIT_VOLATILE_LOAD_FENCE` stays `MFENCE`: it is a diagnostic
    /// opt-in, not a JMM obligation. Page `r11-x64gen-volatile-store-uses-mfence`.
    pub(super) fn emit_volatile_store_fence(&mut self) {
        self.buf.emit(&[0xF0, 0x83, 0x04, 0x24, 0x00]); // LOCK ADD DWORD [RSP], 0
    }

    /// Lower a DEFERRED field site: one call to
    /// `DirectHelperTable::field_site_cp` (interpreter round i4 wave 92, lane
    /// L2; stage 2 of `i50-L1-proposal-compile-doors-resolve-nothing`, the
    /// `i38-L2-proposal-constant-pool-indexed-field-sites` helper; switch
    /// `CRATONVM_JIT_DEFERRED_FIELD_SITES`, default OFF).
    ///
    /// The helper resolves the site through the holder's loader at the program
    /// point (JVMS §5.4.3), runs the §6.5 linking checks (static/instance
    /// kind, the final-field store rule) and the owner's initialisation, and
    /// performs the access, so a hot method whose COLD branch touches a field
    /// of a class not loaded when it compiled is compiled instead of refused.
    /// Nothing is memoised in the code: the site costs a helper call on every
    /// execution (the helper keeps its own per-site memo), which is the price
    /// of a site the interpreter never ran before the compile.
    ///
    /// # The operand block
    ///
    /// The helper can run Java (a user `loadClass`, a `<clinit>`), i.e. a
    /// safepoint, so the operands must stay GC roots across the call and be
    /// read AFTER it. They are homed, still on the abstract stack, in a
    /// contiguous block of frame words reserved BEFORE the safepoint spill
    /// (so the block is inside the live frame and the published homes are
    /// these very words, which a moving collector rewrites in place -- the
    /// monitor receiver's reasoning, `op_object.rs`), and popped only after
    /// the call's oop map is filed. `ARG_REGS[3]` is the block's lowest
    /// address; word `i` is `ops[i]`:
    ///
    /// * `ops[0]`: the writing method's name as an interned NUL-terminated
    ///   string, for the final-field store's `IllegalAccessError` text (`0`
    ///   for a read);
    /// * `ops[1]`: the objectref (`getfield` / `putfield`) or the value
    ///   (`putstatic`);
    /// * `ops[2]`: the value (`putfield`).
    ///
    /// `packed` is `cp_index | opcode << 16 | descriptor tag << 24 | NPE trap
    /// key << 32`. The
    /// result is the loaded value in the bits the resolved helpers return
    /// (`jit_getstatic` / `jit_getfield`), or `i64::MIN` after publishing the
    /// pending exception (a store answers `0`).
    fn emit_deferred_field_site(
        &mut self,
        code: &[u8],
        code_len: usize,
        op: u8,
        pc: usize,
        branch_targets: &[bool],
    ) -> WalkStep {
        let Some(&(holder, cp_idx, tag)) = self.deferred_field_sites.get(&pc) else {
            return WalkStep::Return(unresolved_field_site(pc, op));
        };
        let helper = self.direct_helpers.field_site_cp;
        if helper == 0 || !self.needs_heap {
            return WalkStep::Return(unresolved_field_site(pc, op));
        }
        let operands: usize = match op {
            0xb2 => 0,
            0xb3 | 0xb4 => 1,
            _ => 2,
        };
        if self.stack.len() < operands {
            self.fail("singlepass-codegen/deferred-field-site-stack-underflow");
            return WalkStep::Return(false);
        }
        self.flush_scratch_registers();
        // A withdrawn body leaves BEFORE the helper, at this instruction, its
        // operands still on the stack (as at a deferred `new`).
        self.emit_cp_helper_exit_site(pc, branch_targets.get(pc).copied().unwrap_or(true));
        let words = operands + 1;
        let Some(base) = self.reserve_spill_slots(words, SpillReason::HelperArgs) else {
            return WalkStep::Return(false);
        };
        // Cast: at most three words, a small byte offset.
        let word_off = |i: usize| base + 8 * (words - 1 - i) as i32;
        // ops[0]: the writer's name, for a store.
        let writer_name: i64 = if matches!(op, 0xb3 | 0xb5) {
            let method = self
                .method_key
                .rsplit_once(':')
                .map_or("", |(head, _)| head)
                .rsplit_once('.')
                .map_or("", |(_, name)| name);
            let interned = crate::intern_catch_type_name(&format!("{method}\0"));
            // Cast: a process-lived string's address, carried as a word.
            interned.as_ptr() as usize as i64
        } else {
            0
        };
        self.emit_mov_imm64(RAX, writer_name);
        self.emit_store_local(word_off(0), RAX);
        // ops[1..]: the operands, retargeted to their block words while they
        // stay on the abstract stack (and so in the call's oop map).
        let first = self.stack.len() - operands;
        for k in 0..operands {
            let slot = self.stack[first + k];
            let off = word_off(1 + k);
            self.load_slot_to_reg(RAX, slot);
            self.emit_store_local(off, RAX);
            self.stack[first + k] = StackSlot::Frame(off);
        }
        self.emit_pre_safepoint_spill();
        // A `getfield` / `putfield` with a null objectref raises its NPE in
        // the helper; the trap-site key lets the drain rebuild JEP 358's
        // message from the bytecode at this bci (as `emit_getfield_index_arg`
        // does for a resolved site). `0` = undescribed.
        let npe_key = if matches!(op, 0xb4 | 0xb5) {
            super::inlining::record_npe_trap_site(self.orig_bci(pc))
        } else {
            0
        };
        let packed: u64 = u64::from(cp_idx)
            | (u64::from(op) << 16)
            | (u64::from(tag) << 24)
            | (u64::from(npe_key) << 32);
        self.emit_load_vm_context(ARG_REGS[0]);
        // Cast: the holder word and the packed word are bit patterns.
        self.emit_mov_imm64(ARG_REGS[1], crate::cp_holder_word(holder) as i64);
        self.emit_mov_imm64(ARG_REGS[2], packed as i64);
        self.emit_lea_frame_slot(ARG_REGS[3], word_off(0));
        crate::assert_direct_helper_call_shape!(
            "field_site_cp",
            int_args = 4,
            returns_value = true
        );
        self.emit_call_absolute(helper);
        self.emit_oop_map_for_safepoint();
        for _ in 0..operands {
            self.pop_stack();
        }
        // Hand the block back when nothing at or above it is still live (the
        // block's words are not in stack order, so `pop_stack`'s top-only
        // reclaim cannot), so a method with many deferred sites does not
        // grow its spill area by up to three words per site.
        // Cast: at most three words.
        let block_end = base + 8 * words as i32;
        if self.next_spill_offset == block_end
            && !self
                .stack
                .iter()
                .any(|s| matches!(s, StackSlot::Frame(o) if *o >= base))
        {
            self.next_spill_offset = base;
        }
        let is_get = matches!(op, 0xb2 | 0xb4);
        // A read's sentinel check is its value's (a `J`/`D`/`F` value may be
        // `i64::MIN`'s bits); a store's result is no value.
        self.emit_post_invoke_exception_check(if is_get { tag } else { b'V' });
        if is_get {
            self.push_from_rax();
            if tag == b'L' || tag == b'[' {
                self.mark_top_as_oop();
            }
        }
        // The helper may have run Java (`<clinit>`, a class loader), which can
        // redefine a class this body spliced: a post-call exit site at the next
        // instruction, as after an invoke.
        self.emit_post_call_exit_site(code, code_len, pc + 3, branch_targets);
        WalkStep::Next(pc + 3)
    }

    /// The per-opcode arms behind [`Self::walk_field`].
    #[allow(clippy::too_many_arguments)]
    fn walk_field_arm(
        &mut self,
        code: &[u8],
        _code_len: usize,
        op: u8,
        mut pc: usize,
        _dead: &mut bool,
        _branch_targets: &[bool],
        // `insn_starts` is the whole method's instruction-start map, built
        // once by `compile_bytecode` — see the comment at its definition for
        // why it is a parameter and not a cache, and why it is sized from
        // `code.len()` rather than from `code_len`.
        insn_starts: &[bool],
    ) -> WalkStep {
        // Interpreter round i4 wave 92, lane L2: a site the compile door could
        // not resolve, deferred to its first execution
        // (`BackendRequest::deferred_field_sites`). Such a site has no
        // `field_info` / `static_field_info` row, so every arm below would
        // refuse it (or, for a `getfield`, take the metadata-less helper arm).
        // A scalar-replaced access keeps its arm, which refuses.
        if self.deferred_field_sites.contains_key(&pc) && !self.scalar_field_ops.contains_key(&pc) {
            return self.emit_deferred_field_site(code, _code_len, op, pc, _branch_targets);
        }
        match op {
            // getstatic (0xb2) — a direct load, or the helper
            //
            // HotSpot emits a plain load for a `getstatic`, because both
            // the class and the slot address are known at compile time.
            // CratonVM called `jit_getstatic` for every static read
            // instead, and that CALL — not the read — was the whole cost:
            // ~35 ns against HotSpot's ~1. See
            // `jit-getstatic-costs-a-helper-call-FIXED-20260803.md`.
            //
            // MED-2 (round-2 JIT review) named three blockers for emitting
            // the load. All three are gone:
            //
            //   1. "Slot storage is a `Vec<Value>` inside an `RwLock`ed
            //      `HashMap`, so the address is not stable." It is now a
            //      `StaticsBlock` — one leaked, never-freed allocation per
            //      class — mirrored by the lock-free `StaticsIndex`.
            //   2. "The `Value` enum is a tagged union, not a machine
            //      word." Its layout is pinned by
            //      `types::heap_types::field_cell_layout_matches_value_enum`,
            //      and the inline `getfield` arms have been reading field
            //      cells through `FIELD_CELL_PAYLOAD*_OFFSET` since July.
            //      A static cell is the same 16 bytes.
            //   3. "The `jit` crate has no `vm` dependency, so it cannot
            //      resolve a slot address at compile time." It does not
            //      need one. The VM passes a resolver function pointer
            //      plus its own `SharedVm` pointer on the compile's
            //      `DirectHelperTable`, beside the savebase watch helpers — no
            //      `JitRuntimeHelpers` field, no golden offset, no ABI
            //      revision bump, because generated code never calls it.
            //      Only this backend does, while emitting.
            //
            // What is baked is the address of the class's base-POINTER
            // cell, not of the block: see `try_emit_inline_getstatic` for
            // the emitted shape and `StaticsIndex::base_cell_addr` for why
            // that one extra dependent load buys immunity to every
            // republication path.
            //
            // The helper below still owns every site the resolver declines
            // — a class not yet initialized at compile time (an inline load
            // runs no `<clinit>`), `java/lang/System` (the `out`/`err`/`in`
            // bootstrap intercept), anything not yet published, a second VM
            // in this process, and everything when
            // `CRATONVM_JIT=getstatic-helper` is set.
            //
            // `putstatic` (0xb3) deliberately stays on its helpers; see the
            // note there.
            0xb2 => {
                // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                let (_, class_id_raw, field_index, type_tag, is_volatile) = match self
                    .static_field_info_idx
                    .get(&pc)
                    .map(|&i| self.static_field_info[i])
                {
                    Some(v) => v,
                    None => {
                        if !substitute_unresolved_field_sites() {
                            return WalkStep::Return(unresolved_field_site(pc, 0xb2));
                        }
                        (pc, 0, 0, b'I', false)
                    }
                };

                // Direct load, no helper CALL — the structural fix this
                // opcode's long bail comment above describes. It emits the
                // value push, the oop mark and the volatile fence itself,
                // so the whole helper sequence below is skipped.
                if self.try_emit_inline_getstatic(class_id_raw, field_index, type_tag, is_volatile)
                {
                    pc += 3;
                    return WalkStep::Next(pc);
                }

                self.flush_scratch_registers();
                self.emit_load_vm_context(ARG_REGS[0]);
                self.emit_mov_imm32_sx(ARG_REGS[1], class_id_raw as i32); // Cast: x86-64 immediate encoding
                self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                                                                         // `jit_getstatic` runs `<clinit>` on first touch — arbitrary
                                                                         // Java, so allocation and a collection. That is a safepoint
                                                                         // like any call: spill the register homes and publish a map
                                                                         // for THIS program point. Without the bracket a register-
                                                                         // homed reference local was unreported, and the sp-id slot
                                                                         // still named the previous safepoint's map, which could
                                                                         // claim complete coverage for a frame it no longer described.
                self.emit_pre_safepoint_spill();
                // ABI-3 — the hand-written argument setup above is tied to nothing:
                // `emit_call_absolute` takes a bare address, so appending a parameter
                // to `jit_getstatic` AND to its `helper_fn_slots!` row compiles clean
                // on both sides while this site keeps writing three registers and the
                // callee reads a fourth. The literal below is what this emitter
                // actually writes; the declared arity comes from `HELPER_FN_SIGS`,
                // which is derived from the alias rather than transcribed.
                cratonvm_jit_api::assert_helper_call_shape!(
                    "getstatic",
                    int_args = 3,
                    returns_value = true
                );
                self.emit_call_absolute(self.helpers.getstatic);
                self.emit_oop_map_for_safepoint();
                // jit-linewrapper-flushtype-npe fix (2026-07-17): the
                // helper now runs `<clinit>` on first touch and, on
                // failure, stashes the Java exception and returns the
                // `i64::MIN` deopt sentinel instead of a field value.
                // Route that through the shared exception-check stub
                // (mirrors every other fallible JIT helper call) rather
                // than pushing the sentinel bits as if they were a
                // legitimate result.
                self.emit_post_invoke_exception_check(type_tag);
                // Volatile static: no fence after the read by default -- a
                // plain x86-64 load is already acquire, and the store side
                // keeps its StoreLoad barrier. Opt back in with
                // CRATONVM_JIT_VOLATILE_LOAD_FENCE=1.
                if is_volatile && crate::runtime_lowering::volatile_load_fence_enabled() {
                    self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
                }
                self.push_from_rax();
                // T1.1.a (fix, 2026-07-07 — jit-invokedynamic-groovy
                // regression follow-up): a reference-typed static field's
                // loaded value is a live oop, but `push_from_rax`'s fast
                // path always pushes a hard-coded `false` oop-mark — the
                // same gap already fixed for `getfield`'s inline arms
                // (see the `c_is_ref`/`type_tag == b'L' || b'['` markers
                // above), just never applied here. Left unmarked, this
                // stack slot decodes as a plain non-oop value in any
                // precise GC/deopt oop map built while it's live (e.g. the
                // invokedynamic uncommon-trap snapshot machinery, which
                // records the operand stack directly beneath a live
                // `invokedynamic` call site — `getstatic
                // System.out` immediately followed by a
                // `makeConcatWithConstants` invokedynamic is exactly this
                // shape). A NullPointerException on the following
                // `println` was the concrete symptom (`LicmRepro`/
                // `LicmRepro2`/`ArrRepro`) traced to this exact gap.
                if type_tag == b'L' || type_tag == b'[' {
                    self.mark_top_as_oop();
                }
                // The helper may have run `<clinit>`, Java code that can
                // redefine a class this body spliced: a post-call exit site
                // at the next instruction, as after an invoke
                // (`op_invoke.rs::POST_CLINIT_HELPER_EXIT_SITES`).
                self.emit_post_clinit_helper_exit_site(
                    code,
                    _code_len,
                    pc + 3,
                    _branch_targets,
                    class_id_raw,
                );
                pc += 3;
            }

            // putstatic (0xb3) — write static field via type-specific helper
            //
            // The three MED-2 blockers listed on the 0xb2 arm above are
            // gone, and the same baked base-pointer cell would address a
            // write just as well. The write side is NOT symmetric, though,
            // and deliberately stays on the helper: `set_static_shared`
            // fires the SATB pre-barrier for an overwritten reference —
            // statics live in this Rust-side table, not the heap, so no
            // collector `set_field` barrier covers them and a missed one is
            // a hidden-pointer SATB hole (final remark misses the old
            // referent, cleanup frees a live region) — and it is also what
            // creates or grows a class's block on first touch. Inlining
            // reads costs that machinery nothing; inlining writes would
            // have to reproduce all of it. Reads were the measured problem
            // (see the 0xb2 arm's doc); a primitive-only inline `putstatic`
            // is the tractable next step if static WRITES ever show up on a
            // hot path.
            0xb3 => 'putstatic: {
                self.flush_scratch_registers();
                // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                let (_, class_id_raw, field_index, type_tag, is_volatile) = match self
                    .static_field_info_idx
                    .get(&pc)
                    .map(|&i| self.static_field_info[i])
                {
                    Some(v) => v,
                    None => {
                        if !substitute_unresolved_field_sites() {
                            return WalkStep::Return(unresolved_field_site(pc, 0xb3));
                        }
                        (pc, 0, 0, b'I', false)
                    }
                };

                let val_slot = self.pop_stack();

                // Round 16 (lane fieldguard, `CRATONVM_JIT_INLINE_PUTSTATIC`,
                // default ON since round 16): a primitive static of an initialized class is a
                // direct store, no helper (see `try_emit_inline_putstatic`).
                // Never for a substituted (unresolved) site.
                if self.static_field_info_idx.contains_key(&pc)
                    && self.try_emit_inline_putstatic(class_id_raw, field_index, type_tag, val_slot)
                {
                    if is_volatile {
                        self.emit_volatile_store_fence();
                    }
                    pc += 3;
                    break 'putstatic;
                }

                // Select the appropriate helper based on type_tag
                let helper_fn: usize = match type_tag {
                    b'J' => self.helpers.putstatic_long,
                    b'F' => self.helpers.putstatic_float,
                    b'D' => self.helpers.putstatic_double,
                    b'L' | b'[' => self.helpers.putstatic_object,
                    _ => self.helpers.putstatic_int,
                };

                // Volatile static: NO leading fence. A volatile store needs
                // StoreStore ahead of it and StoreLoad behind it; x86-64 is TSO, so
                // StoreStore (and LoadStore) are free and only the trailing barrier
                // below has to be emitted. This arm used to emit MFENCE on BOTH sides
                // — the only volatile-store site in the backend that did. The
                // volatile `putfield` path has always emitted the trailing barrier
                // alone — `walk_field` emits it AFTER the `0xb5` arm returns — and
                // HotSpot likewise emits one StoreLoad barrier after a volatile store.
                // (The other MFENCE site in this file, in the `getstatic` arm above,
                // is a volatile LOAD fence, default OFF behind
                // `CRATONVM_JIT_VOLATILE_LOAD_FENCE`; it is not a store barrier.)
                // Removed in r10: same ordering, one fewer serializing instruction on
                // every volatile static write. Found by lane `ops`, which correctly
                // declined to change it without this reasoning written down.
                // Call jit_putstatic_xxx(vm_ptr, class_id_raw, field_index, val)
                self.emit_load_vm_context(ARG_REGS[0]); // vm_ptr
                self.emit_mov_imm32_sx(ARG_REGS[1], class_id_raw as i32); // class_id // Cast: x86-64 immediate encoding
                self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // field_index // Cast: x86-64 immediate encoding
                self.load_slot_to_reg(ARG_REGS[3], val_slot); // value
                                                              // JVMS §6.5 `putstatic`: a `boolean` static stores `value & 1`,
                                                              // and a `byte`/`char`/`short` static only its own width — the
                                                              // same narrowing the `0xb5` arm and the interpreter's
                                                              // `pop_static_field_value` apply. `jit_putstatic_int` stores
                                                              // its operand verbatim and has no descriptor to narrow by, so
                                                              // it happens here. A no-op for every tag but Z/B/C/S.
                self.emit_narrow_to_field_tag(ARG_REGS[3], type_tag);
                // `<clinit>` on first touch, as for `getstatic`: a safepoint.
                self.emit_pre_safepoint_spill();
                // One sequence serves five slots, so all five are asserted rather
                // than four of them being "the same shape as the one above". Each
                // takes (vm_ptr, class_id, field_index, value) and returns the
                // `i64::MIN` deopt sentinel when `<clinit>` must run, which is why
                // `returns_value` is true for the `-> ()`-looking store opcodes.
                cratonvm_jit_api::assert_helper_call_shape!(
                    "putstatic_int",
                    int_args = 4,
                    returns_value = true
                );
                cratonvm_jit_api::assert_helper_call_shape!(
                    "putstatic_long",
                    int_args = 4,
                    returns_value = true
                );
                cratonvm_jit_api::assert_helper_call_shape!(
                    "putstatic_float",
                    int_args = 4,
                    returns_value = true
                );
                cratonvm_jit_api::assert_helper_call_shape!(
                    "putstatic_double",
                    int_args = 4,
                    returns_value = true
                );
                cratonvm_jit_api::assert_helper_call_shape!(
                    "putstatic_object",
                    int_args = 4,
                    returns_value = true
                );
                self.emit_call_absolute(helper_fn);
                self.emit_oop_map_for_safepoint();
                // jit-putstatic-clinit-gap fix (2026-07-17): see the
                // matching comment at the inlined-callee 0xb3 arm above —
                // same helper, same new fallible-`<clinit>` sentinel.
                self.emit_post_invoke_exception_check(b'V');
                // Volatile static: the StoreLoad barrier after the write.
                if is_volatile {
                    self.emit_volatile_store_fence();
                }
                // As after the `getstatic` helper: `<clinit>` may have run.
                self.emit_post_clinit_helper_exit_site(
                    code,
                    _code_len,
                    pc + 3,
                    _branch_targets,
                    class_id_raw,
                );
                pc += 3;
            }

            // getfield — read object field via helper (or frame slot for scalar-replaced)
            0xb4 => {
                if let Some(&new_pc) = self.scalar_field_ops.get(&pc) {
                    // Scalar-replaced getfield: load directly from frame slot
                    // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                    let (_, field_index, type_tag) =
                        match self.field_info_idx.get(&pc).map(|&i| self.field_info[i]) {
                            Some(v) => v,
                            None => {
                                if !substitute_unresolved_field_sites() {
                                    return WalkStep::Return(unresolved_field_site(pc, 0xb4));
                                }
                                (pc, 0, b'I')
                            }
                        };
                    let _obj_slot = self.pop_stack(); // dummy objectref
                    let sr_obj = &self.scalar_replaced[&new_pc];
                    let field_off =
                        sr_obj.field_base_offset + (field_index as i32) * (SLOT_SIZE as i32); // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, field_off);
                    self.push_from_rax();
                    // A REFERENCE field of a scalar-replaced object is an
                    // ordinary live oop once loaded — the object being
                    // exploded into frame slots changes where the field
                    // lives, not what its value is. Same obligation as every
                    // other `getfield` arm.
                    if type_tag == b'L' || type_tag == b'[' {
                        self.mark_top_as_oop();
                    }
                    pc += 3;
                } else if let Some(&(c_off, c_is_ref)) =
                    self.compact_field_off.get(&pc).filter(|_| {
                        !narrow_oops_block_inline_fields()
                            && (inline_getfield_enabled()
                                || (guarded_inline_getfield_enabled()
                                    && self.helpers.read_bounds_addr != 0))
                    })
                {
                    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_COMPACT_INLINE") {
                        eprintln!("[compact-inline] getfield pc={pc} off={c_off} ref={c_is_ref}");
                    }
                    // Compact reference-field layout inline getfield. The
                    // packed byte offset + ref-ness were resolved at compile
                    // time, so emit a raw MOV (no helper call, no runtime
                    // layout lookup). A reference field is the bare 8-byte
                    // pointer AT the field; primitives are their tagless
                    // descriptor width (1/2/4/8 bytes).
                    //
                    // CRITICAL: a class with a registered compact layout may
                    // still have LEGACY-laid-out (16-byte-cell) instances —
                    // any allocation whose `num_fields` disagrees with the
                    // layout's field count falls back to the uniform layout
                    // (`plan_object_alloc`), e.g. native/synthetic-stub
                    // allocations of `java/lang/reflect/Method`,
                    // `ConcurrentHashMap`, `ArrayList`, … whose stub field
                    // count exceeds the real declared count the layout was
                    // built from. The compact offset is only valid for a
                    // genuinely-compact object, so we MUST key on the
                    // per-object `GC_FLAG_COMPACT` header bit (byte 7, `GC_FLAGS_BYTE_OFFSET`) the
                    // same way every heap/helper access path does — reading a
                    // legacy object at the compact offset returns a mangled
                    // {tag,partial-pointer} word that SIGSEGVs when later
                    // dereferenced/called. For a legacy receiver we take the
                    // uniform `index * SLOT_SIZE` 16-byte-cell path inline.
                    let (_, field_index, type_tag) =
                        match self.field_info_idx.get(&pc).map(|&i| self.field_info[i]) {
                            Some(v) => v,
                            None => {
                                if !substitute_unresolved_field_sites() {
                                    return WalkStep::Return(unresolved_field_site(pc, 0xb4));
                                }
                                (pc, 0, b'I')
                            }
                        };
                    let cell_off = c_off as i32; // Cast: an absolute compact displacement, x86-64 disp32
                    let legacy_cell_off = (HEADER_SIZE + field_index * SLOT_SIZE) as i32; // Cast: disp32
                                                                                          // GUARDED (default) vs RAW (CRATONVM_JIT_INLINE_GETFIELD):
                                                                                          // raw keeps the historical null→0 inline semantics; guarded
                                                                                          // routes null/implausible receivers to the checked helper
                                                                                          // (NPE + i64::MIN sentinel) and flushes the scratch cache
                                                                                          // up-front because its slow path CALLs out.
                    let raw_mode = inline_getfield_enabled();
                    // Both modes flush since 2026-09-10: the
                    // layout-replacement guard below clobbers R11 and RCX
                    // on its fallback form, and its bail CALLs the checked
                    // helper — so raw mode has a call on a path where it
                    // never had one, and a stale scratch cache across it
                    // would hand a later read a register the callee owns.
                    self.flush_scratch_registers();
                    let trusted_have_key = !self.method_key.is_empty();
                    let trusted_marks_exact = self.stack_oop_marks_exact;
                    let trusted_top_is_oop = self.stack_oop_marks.last().copied().unwrap_or(false);
                    let receiver_is_trusted_oop =
                        trusted_have_key && trusted_marks_exact && trusted_top_is_oop;
                    // WHICH of the three clauses refused, at EMISSION time.
                    //
                    // The trusted-oop shortcut is the difference between a
                    // bare null test and the six-compare containment check
                    // that no non-publishing collector can ever pass — i.e.
                    // between an inline load and a helper call, on the
                    // hottest path in the VM. The getfield page's open item
                    // 1 is "why is `stack_oop_marks_exact` false at these
                    // sites", and until now the only way to ask was to read
                    // the disassembly and infer. A bare "not trusted" would
                    // repeat this page's own founding mistake: it names a
                    // verdict, not a cause, and the three clauses want
                    // completely different fixes.
                    if !receiver_is_trusted_oop
                        && cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_COMPACT_INLINE")
                    {
                        eprintln!(
                            "[compact-inline] getfield pc={pc} NOT-trusted-oop \
                             have_key={trusted_have_key} marks_exact={trusted_marks_exact} \
                             top_is_oop={trusted_top_is_oop} depth={} method={}",
                            self.stack.len(),
                            self.method_key,
                        );
                    }
                    let obj_slot = self.pop_stack();
                    // The baked `cell_off` is a compile-time claim about
                    // this class's compact layout, and the class manager
                    // can REPLACE that layout at run time — the
                    // synthetic-stub→real-bytecode upgrade. Every other
                    // emitter that bakes one has guarded it since
                    // 2026-09-04, whose commit named five such sites; this
                    // one and the ungated compact reference `putfield`
                    // below were not among them, and read at the OLD
                    // offset for a REPLACED layout with nothing to stop
                    // them. The allocation emitters' own comment calls
                    // that "confirmed heap corruption".
                    //
                    // Emitted BEFORE the receiver load because the
                    // fallback form clobbers R11 and RCX, exactly as
                    // `objects.rs`'s three call sites do.
                    let mut layout_bail: Vec<usize> = if jit_sp_field_layout_guard_enabled() {
                        self.emit_layout_epoch_guard().into_iter().collect()
                    } else {
                        Vec::new()
                    };
                    self.load_slot_to_reg(RAX, obj_slot);
                    let (mut slow_patches, null_patch) = if raw_mode {
                        // Null check: TEST RAX,RAX; JZ <null> (result 0).
                        self.emit_test_r64_r64(RAX);
                        (Vec::new(), Some(self.emit_jcc_rel32_patch(0x84))) // JE
                    } else if receiver_is_trusted_oop {
                        (
                            self.emit_trusted_oop_receiver_check_at(code, pc, true, 0, insn_starts),
                            None,
                        )
                    } else {
                        (
                            self.emit_guarded_getfield_receiver_check(
                                self.helpers.read_bounds_addr,
                            ),
                            None,
                        )
                    };
                    // Per-object compactness: gc_flags byte (`GC_FLAGS_BYTE_OFFSET`,
                    // 7) & GC_FLAG_COMPACT. Zero ⇒ legacy 16-byte-cell object →
                    // uniform-layout read. A DWORD load on purpose: it is the
                    // instruction the implicit null check faults on, and
                    // `bind_implicit_null_recovery` accepts only `MOV r32,
                    // [RAX+disp32]`. Bytes 8..10 it also reads are a compact
                    // instance's first field (or a long header's shape word),
                    // inside every object (`MIN_OBJECT_SIZE` is 16); only the
                    // flag bit survives the AND.
                    self.emit_mov_r32_mem_disp32(
                        RCX,
                        RAX,
                        cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                    );
                    self.emit_and_r64_imm8(RCX, cratonvm_types::GC_FLAG_COMPACT as i8);
                    let legacy_patch = self.emit_jcc_rel32_patch(0x84); // JZ → legacy
                                                                        // --- compact path (8-byte ref / packed primitive cell) ---
                    if c_is_ref {
                        // 8-byte raw pointer at the cell base.
                        self.emit_mov_r64_mem_disp32(RAX, RAX, cell_off);
                    } else {
                        match type_tag {
                            b'J' | b'D' => {
                                self.emit_mov_r64_mem_disp32(RAX, RAX, cell_off);
                            }
                            b'L' | b'[' => {
                                // Defense-in-depth: contradictory metadata
                                // (`c_is_ref == false` for a reference
                                // descriptor). A non-ref-classified slot is
                                // written as a full 16-byte `Value` cell, so
                                // read the 8-byte pointer payload — never the
                                // 32-bit MOVSXD below, which sign-extends half
                                // a pointer into a bogus non-null receiver.
                                self.emit_mov_r64_mem_disp32(
                                    RAX,
                                    RAX,
                                    cell_off + FIELD_CELL_PAYLOAD64_OFFSET as i32,
                                );
                            }
                            b'F' => {
                                self.emit_mov_r32_mem_disp32(RAX, RAX, cell_off);
                            }
                            b'Z' => self.emit_movx_r64_mem_disp32(RAX, RAX, cell_off, 8, false),
                            b'B' => self.emit_movx_r64_mem_disp32(RAX, RAX, cell_off, 8, true),
                            b'C' => self.emit_movx_r64_mem_disp32(RAX, RAX, cell_off, 16, false),
                            b'S' => self.emit_movx_r64_mem_disp32(RAX, RAX, cell_off, 16, true),
                            _ => {
                                self.emit_movsxd_r64_mem_disp32(RAX, RAX, cell_off);
                            }
                        }
                    }
                    let done_compact_patch = self.emit_jmp_rel32_patch();
                    // --- legacy path (uniform 16-byte Value cell) ---
                    // Mirrors the non-compact inline getfield arm: a reference
                    // (or long/double) field is the 8-byte payload at
                    // `legacy_cell + PAYLOAD64`; float is 4 bytes at PAYLOAD32;
                    // int-category is a sign-extended 4-byte load at PAYLOAD32.
                    self.patch_rel32_to_here(legacy_patch);
                    // Before either reference arm below reads the cell's
                    // 8-byte payload as a POINTER, check that the cell says
                    // it holds one. The IR backend's twin of this sequence
                    // did not, and a `[C` field whose cell held
                    // `Value::Int(1)` was loaded as the pointer `1` and
                    // dereferenced by the `arraylength` three instructions
                    // later -- SIGSEGV at `addr=0x5`, deterministically
                    // (Tomcat/Derby, 2026-08-23). The checked helper has
                    // always looked at the variant; this arm exists to skip
                    // the helper, so it has to ask the same question.
                    //
                    // RAW mode is excluded because it has no slow path to
                    // defer to -- its null arm yields 0 by design. It is an
                    // explicit opt-in whose own comment calls itself
                    // "historical semantics"; the guarded path is the
                    // default and is what the crash was on.
                    if !raw_mode && (c_is_ref || matches!(type_tag, b'L' | b'[')) {
                        self.buf.emit(&[0x83, 0xB8]); // CMP DWORD [RAX+disp32], imm8
                        self.buf
                            .emit(&(legacy_cell_off + FIELD_CELL_TAG_OFFSET as i32).to_le_bytes());
                        self.buf
                            .emit_byte(cratonvm_types::FIELD_CELL_TAG_OBJECT as u8);
                        slow_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNE → helper
                    }
                    if c_is_ref {
                        self.emit_mov_r64_mem_disp32(
                            RAX,
                            RAX,
                            legacy_cell_off + FIELD_CELL_PAYLOAD64_OFFSET as i32,
                        );
                    } else {
                        match type_tag {
                            b'J' | b'D' => {
                                self.emit_mov_r64_mem_disp32(
                                    RAX,
                                    RAX,
                                    legacy_cell_off + FIELD_CELL_PAYLOAD64_OFFSET as i32,
                                );
                            }
                            b'L' | b'[' => {
                                // Defense-in-depth (see the compact branch):
                                // a reference descriptor always reads the
                                // legacy cell's 64-bit pointer payload, even
                                // when `c_is_ref` wrongly says non-ref.
                                self.emit_mov_r64_mem_disp32(
                                    RAX,
                                    RAX,
                                    legacy_cell_off + FIELD_CELL_PAYLOAD64_OFFSET as i32,
                                );
                            }
                            b'F' => {
                                self.emit_mov_r32_mem_disp32(
                                    RAX,
                                    RAX,
                                    legacy_cell_off + FIELD_CELL_PAYLOAD32_OFFSET as i32,
                                );
                            }
                            _ => {
                                self.emit_movsxd_r64_mem_disp32(
                                    RAX,
                                    RAX,
                                    legacy_cell_off + FIELD_CELL_PAYLOAD32_OFFSET as i32,
                                );
                            }
                        }
                    }
                    let done_legacy_patch = self.emit_jmp_rel32_patch();
                    // RAW mode emits no helper tail of its own, but the
                    // layout-replacement bail above needs one: routing it
                    // to the null path would answer 0, and routing it to
                    // the legacy arm would read a compact object at the
                    // uniform slot offset. So raw mode grows the tail too,
                    // and jumps over it on the null path.
                    let mut done_null_patch = None;
                    if let Some(null_patch) = null_patch {
                        // RAW mode null path: RAX := 0 (historical semantics).
                        self.patch_rel32_to_here(null_patch);
                        self.emit_xor_reg_self(RAX);
                        if !layout_bail.is_empty() {
                            done_null_patch = Some(self.emit_jmp_rel32_patch());
                        }
                    }
                    if null_patch.is_none() || !layout_bail.is_empty() {
                        // GUARDED slow path: null / unaligned / out-of-heap
                        // receiver → the checked helper, whose NPE +
                        // i64::MIN-sentinel semantics match the helper-only
                        // arm below exactly. A replaced layout lands here
                        // too, from either mode: the helper resolves the
                        // CURRENT layout, which is the whole point of
                        // refusing the baked offset.
                        for p in slow_patches {
                            self.patch_rel32_to_here(p);
                        }
                        for p in layout_bail.drain(..) {
                            self.patch_rel32_to_here(p);
                        }
                        // L1W19-2 (lane qNull): whether the bind below makes
                        // this slow path an implicit null check's recovery
                        // address, so its helper call can carry the mark the
                        // VM counts faults by.
                        let implicit_bound =
                            null_patch.is_none() && !self.implicit_null_pending.is_empty();
                        if null_patch.is_none() {
                            // The implicit null check's recovery address is
                            // THIS point. The slow path reloads the receiver
                            // from its frame slot rather than reusing RAX, so
                            // a fault recovered into here needs no register
                            // repair — only the instruction pointer moves.
                            //
                            // Raw mode tested null explicitly and has no
                            // implicit check to recover, so it must not
                            // claim this address as one.
                            self.bind_implicit_null_recovery();
                        }
                        self.emit_load_vm_context(ARG_REGS[0]);
                        self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                        let site_mark = self.implicit_null_site_mark(implicit_bound);
                        self.emit_getfield_index_arg_marked(
                            ARG_REGS[2],
                            field_index,
                            type_tag,
                            pc,
                            site_mark,
                        );
                        crate::metrics::note_getfield_arm(1);
                        // getfield arm 1 — (vm_ptr, obj_ptr, field_index).
                        cratonvm_jit_api::assert_helper_call_shape!(
                            "getfield",
                            int_args = 3,
                            returns_value = true
                        );
                        self.emit_call_absolute(self.helpers.getfield);
                        self.emit_post_invoke_exception_check(type_tag);
                    }
                    // join
                    if let Some(p) = done_null_patch {
                        self.patch_rel32_to_here(p);
                    }
                    self.patch_rel32_to_here(done_compact_patch);
                    self.patch_rel32_to_here(done_legacy_patch);
                    self.push_from_rax();
                    // T1.1.a — a reference field's loaded value is a live
                    // oop; the `push_from_rax` fast path always pushes a
                    // hard-coded `false` mark, which left this getfield
                    // result (and everything computed from it) unrooted
                    // for the GC / precise-deopt oop map. `c_is_ref` is
                    // authoritative here regardless of which sub-path
                    // (compact or legacy cell) actually loaded it.
                    if c_is_ref {
                        self.mark_top_as_oop();
                    }
                    pc += 3;
                } else if let Some(&info_idx) = self
                    .field_info_idx
                    .get(&pc)
                    // RAW inline (opt-in) only when the compact layout is
                    // globally off: a compact receiver's reference field is an
                    // 8-byte pointer and a primitive field sits at a packed
                    // offset, so the baked `HEADER + index*SLOT_SIZE`
                    // 16-byte-cell load is wrong for it. The GUARDED default
                    // handles compact-on by testing the per-object
                    // GC_FLAG_COMPACT bit and routing compact receivers to the
                    // compact-aware `jit_getfield` helper.
                    .filter(|_| {
                        (inline_getfield_enabled() && !cratonvm_types::compact_ref_fields_enabled())
                            || (guarded_inline_getfield_enabled()
                                && self.helpers.read_bounds_addr != 0)
                    })
                {
                    // Inline field load — the field index and type tag are
                    // statically resolved (`field_info` was built from
                    // `resolve_field_ref` in lib.rs), so we can emit a raw
                    // MOV against the object's field cell instead of a
                    // `CALL jit_getfield`. Bit-identical to the helper:
                    //
                    //   * null receiver  → result 0  (the helper's
                    //     `if obj_ptr == 0 { return 0 }` guard);
                    //   * int-category   → MOVSXD (sign-extend, matches
                    //     `Value::Int(i) => i as i64`);
                    //   * float          → 32-bit MOV (zero-extend, matches
                    //     `Value::Float(f) => f.to_bits() as i64`);
                    //   * long/double/ref → 64-bit MOV of the payload word
                    //     (`Long`/`Double` bits, or the raw object pointer
                    //     which is 0 for `Object(None)`).
                    //
                    // The field cell is the 16-byte `Value` enum; payload
                    // offsets within the cell come from the
                    // `FIELD_CELL_PAYLOAD*_OFFSET` constants in `types`
                    // (layout pinned by `field_cell_layout_matches_value_enum`).
                    let (_, field_index, type_tag) = self.field_info[info_idx];
                    let cell_off = (HEADER_SIZE + field_index * SLOT_SIZE) as i32; // Cast: x86-64 disp32
                                                                                   // GUARDED (default) vs RAW (opt-in, compact-off only) —
                                                                                   // see the compact arm above for the mode contract.
                    let raw_mode =
                        inline_getfield_enabled() && !cratonvm_types::compact_ref_fields_enabled();
                    if !raw_mode {
                        self.flush_scratch_registers();
                    }
                    let receiver_is_trusted_oop = !self.method_key.is_empty()
                        && self.stack_oop_marks_exact
                        && self.stack_oop_marks.last().copied().unwrap_or(false);
                    let obj_slot = self.pop_stack();
                    // Receiver → RAX.
                    self.load_slot_to_reg(RAX, obj_slot);
                    let (mut slow_patches, null_patch) = if raw_mode {
                        // Null check: TEST RAX,RAX; JZ <null-path>. On null we
                        // skip the load entirely and leave RAX = 0, matching
                        // `jit_getfield`'s early `return 0`.
                        self.emit_test_r64_r64(RAX);
                        (Vec::new(), Some(self.emit_jcc_rel32_patch(0x84))) // JE
                    } else if receiver_is_trusted_oop {
                        (
                            // Opts in exactly when the guard below emits
                            // its `GC_FLAGS` read at `[RAX + 7]`, which is
                            // the receiver dereference the implicit check
                            // faults on. `raw_mode` is already false in this
                            // branch -- it is the `if raw_mode` arm's
                            // sibling -- so `!raw_mode && compact` reduces
                            // to `compact` and the two conditions are the
                            // same expression rather than two that have to
                            // be kept in step.
                            //
                            // They are still verified independently:
                            // `bind_implicit_null_recovery` decodes the
                            // bytes actually emitted at the recorded offset
                            // and fails the compile if they are not that
                            // load. This predicate being wrong costs a
                            // refused compile, not a missing null check.
                            self.emit_trusted_oop_receiver_check_at(
                                code,
                                pc,
                                cratonvm_types::compact_ref_fields_enabled(),
                                1,
                                insn_starts,
                            ),
                            None,
                        )
                    } else {
                        (
                            self.emit_guarded_getfield_receiver_check(
                                self.helpers.read_bounds_addr,
                            ),
                            None,
                        )
                    };
                    if !raw_mode && cratonvm_types::compact_ref_fields_enabled() {
                        // Per-object compact receiver → helper: this pc has no
                        // registered compact offset (or the class layout didn't
                        // match), so the uniform 16-byte-cell load below is only
                        // valid for a legacy-laid-out object. gc_flags byte @7.
                        self.emit_mov_r32_mem_disp32(
                            RCX,
                            RAX,
                            cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                        );
                        self.emit_and_r64_imm8(RCX, cratonvm_types::GC_FLAG_COMPACT as i8);
                        slow_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNZ
                    }
                    // A REFERENCE read must check the cell's discriminant
                    // before treating its payload as a pointer. `J`, `D`,
                    // `L` and `[` all share the 8-byte payload offset, so
                    // the DESCRIPTOR does not tell you what the cell
                    // actually holds -- only the tag does. Reading it
                    // without asking is the Tomcat/Derby SIGSEGV of
                    // 2026-08-23; see FIELD_CELL_TAG_OBJECT and the twin
                    // check in the compact arm above.
                    //
                    // RAW mode is excluded because it has no slow path to
                    // defer to; it is an explicit opt-in that documents
                    // itself as historical semantics.
                    if !raw_mode && matches!(type_tag, b'L' | b'[') {
                        self.buf.emit(&[0x83, 0xB8]); // CMP DWORD [RAX+disp32], imm8
                        self.buf
                            .emit(&(cell_off + FIELD_CELL_TAG_OFFSET as i32).to_le_bytes());
                        self.buf
                            .emit_byte(cratonvm_types::FIELD_CELL_TAG_OBJECT as u8);
                        slow_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNE → helper
                    }
                    match type_tag {
                        b'J' | b'D' | b'L' | b'[' => {
                            // 8-byte payload: MOV RAX, [RAX + cell + 8].
                            self.emit_mov_r64_mem_disp32(
                                RAX,
                                RAX,
                                // Cast: fixed struct/layout offset to i32 instruction displacement
                                cell_off + FIELD_CELL_PAYLOAD64_OFFSET as i32,
                            );
                        }
                        b'F' => {
                            // 4-byte float bits: MOV EAX (zero-extends).
                            self.emit_mov_r32_mem_disp32(
                                RAX,
                                RAX,
                                // Cast: fixed struct/layout offset to i32 instruction displacement
                                cell_off + FIELD_CELL_PAYLOAD32_OFFSET as i32,
                            );
                        }
                        _ => {
                            // int / boolean / byte / char / short: MOVSXD
                            // (sign-extend) — `Value::Int` payload.
                            self.emit_movsxd_r64_mem_disp32(
                                RAX,
                                RAX,
                                // Cast: fixed struct/layout offset to i32 instruction displacement
                                cell_off + FIELD_CELL_PAYLOAD32_OFFSET as i32,
                            );
                        }
                    }
                    let done_patch = self.emit_jmp_rel32_patch();
                    if let Some(null_patch) = null_patch {
                        // RAW mode null path: RAX := 0.
                        self.patch_rel32_to_here(null_patch);
                        self.emit_xor_reg_self(RAX);
                    } else {
                        // GUARDED slow path: null / unaligned / out-of-heap /
                        // compact-flagged receiver → the checked helper (same
                        // NPE + sentinel semantics as the helper-only arm).
                        for p in slow_patches {
                            self.patch_rel32_to_here(p);
                        }
                        // The implicit null check's recovery address, as in
                        // the compact arm: this slow path reloads the
                        // receiver from its frame slot rather than reusing
                        // RAX, so a recovered fault needs no register
                        // repair — only the instruction pointer moves.
                        // L1W19-2 (lane qNull): bound here => mark the call.
                        let implicit_bound = !self.implicit_null_pending.is_empty();
                        self.bind_implicit_null_recovery();
                        self.emit_load_vm_context(ARG_REGS[0]);
                        self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                        let site_mark = self.implicit_null_site_mark(implicit_bound);
                        self.emit_getfield_index_arg_marked(
                            ARG_REGS[2],
                            field_index,
                            type_tag,
                            pc,
                            site_mark,
                        );
                        crate::metrics::note_getfield_arm(2);
                        // getfield arm 2 — (vm_ptr, obj_ptr, field_index).
                        cratonvm_jit_api::assert_helper_call_shape!(
                            "getfield",
                            int_args = 3,
                            returns_value = true
                        );
                        self.emit_call_absolute(self.helpers.getfield);
                        self.emit_post_invoke_exception_check(type_tag);
                    }
                    // Join: result in RAX.
                    self.patch_rel32_to_here(done_patch);
                    self.push_from_rax();
                    // T1.1.a — see the compact-getfield arm above: a
                    // reference-typed field's loaded value is a live oop
                    // and must not be left with the default `false` mark
                    // `push_from_rax` assigns, or it decodes as a plain
                    // `Int` (not `Object`) in the precise GC/deopt oop
                    // map — the root cause of the JDT `Parser`
                    // stack-corruption bug (jasper-jdt-parser-arrayindexoutofbounds.md): a
                    // `char[][]` field read this way, then used live
                    // across an always-deopting `System.arraycopy`
                    // reference-array call, resumed in the interpreter as
                    // a raw integer instead of an object reference.
                    if type_tag == b'L' || type_tag == b'[' {
                        self.mark_top_as_oop();
                    }
                    pc += 3;
                } else if let Some(&info_idx) = self.field_info_idx.get(&pc) {
                    // Statically resolved field, but the raw inline path
                    // is disabled. Route through the checked helper while
                    // preserving the resolved slot index and oop marking.
                    let (_, field_index, type_tag) = self.field_info[info_idx];
                    self.flush_scratch_registers();
                    let obj_slot = self.pop_stack();
                    self.emit_load_vm_context(ARG_REGS[0]);
                    self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                    self.emit_getfield_index_arg(ARG_REGS[2], field_index, type_tag, pc);
                    crate::metrics::note_getfield_arm(3);
                    // getfield arm 3 — (vm_ptr, obj_ptr, field_index).
                    cratonvm_jit_api::assert_helper_call_shape!(
                        "getfield",
                        int_args = 3,
                        returns_value = true
                    );
                    self.emit_call_absolute(self.helpers.getfield);
                    // See the inlined-callee getfield site above: the checked
                    // helper's `i64::MIN` sentinel must be caught here, before
                    // it's pushed/tagged as a live value, or a bad receiver
                    // silently corrupts execution instead of throwing.
                    self.emit_post_invoke_exception_check(type_tag);
                    self.push_from_rax();
                    if type_tag == b'L' || type_tag == b'[' {
                        self.mark_top_as_oop();
                    }
                    pc += 3;
                } else {
                    // No statically-resolved field metadata for this
                    // getfield PC — fall back to the runtime helper. The
                    // field's real type is unknown here, so pass `b'J'` to
                    // force the ambiguity-safe (peek-dispatch_threw) sentinel
                    // check unconditionally — safe for every type, since a
                    // non-J/D/F field can never legitimately produce
                    // `i64::MIN` anyway.
                    self.flush_scratch_registers();
                    let obj_slot = self.pop_stack();
                    self.emit_load_vm_context(ARG_REGS[0]);
                    self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                    // NOT routed through `emit_getfield_index_arg`: that
                    // encoder needs a type tag, and this arm is the one
                    // that has none. So `GETFIELD_EXPECT_REFERENCE` cannot
                    // be set here and the helper keeps its pre-2026-08-23
                    // behaviour of returning a punned slot's payload. That
                    // is survivable only because the same missing metadata
                    // means this arm does not know the value is a reference
                    // either, so nothing downstream dereferences it on the
                    // strength of a static type -- but it is a residual,
                    // and the place to close it is the resolution that
                    // failed, not here.
                    self.emit_mov_imm32_sx(ARG_REGS[2], 0); // Cast: x86-64 immediate encoding
                    crate::metrics::note_getfield_arm(4);
                    // getfield arm 4 — (vm_ptr, obj_ptr, field_index).
                    cratonvm_jit_api::assert_helper_call_shape!(
                        "getfield",
                        int_args = 3,
                        returns_value = true
                    );
                    self.emit_call_absolute(self.helpers.getfield);
                    self.emit_post_invoke_exception_check(b'J');
                    self.push_from_rax();
                    pc += 3;
                }
            }

            // putfield — write object field (frame slot for scalar-replaced, helper otherwise)
            0xb5 => {
                if let Some(&new_pc) = self.scalar_field_ops.get(&pc) {
                    // Scalar-replaced putfield: store value directly to frame slot
                    // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                    let (_, field_index, type_tag) =
                        match self.field_info_idx.get(&pc).map(|&i| self.field_info[i]) {
                            Some(v) => v,
                            None => {
                                if !substitute_unresolved_field_sites() {
                                    return WalkStep::Return(unresolved_field_site(pc, 0xb5));
                                }
                                (pc, 0, b'I')
                            }
                        };
                    note_field_site(&self.method_key, "putfield/scalar", pc, field_index, b'?');
                    let val_slot = self.pop_stack();
                    let _obj_slot = self.pop_stack(); // dummy objectref
                    let sr_obj = &self.scalar_replaced[&new_pc];
                    let field_off =
                        sr_obj.field_base_offset + (field_index as i32) * (SLOT_SIZE as i32); // Cast: x86-64 immediate encoding
                    self.load_slot_to_reg(RAX, val_slot);
                    // A sub-int field keeps only its own width (r9-ea). The
                    // interpreter narrows every `putfield` to a `B`/`S`/`C`/`Z`
                    // field (`narrow_int_to_field_type`) and a heap object's
                    // compact cell is that width anyway; this frame slot is a
                    // full qword, so without the narrowing a later `getfield`
                    // of the scalar object read back an int the field cannot
                    // hold (and a deopt materialised it into the object).
                    // javac always narrows first (`i2b`, `i2s`, `i2c`, a 0/1
                    // boolean), so for its bytecode each of these is a no-op
                    // on the value — one register instruction.
                    self.emit_narrow_to_field_tag(RAX, type_tag);
                    self.emit_store_local(field_off, RAX);
                    // Each scalar field reserves SLOT_SIZE bytes (see `new` zero-init). Always
                    // clear the high qword so category-1 values and refs never leave garbage in
                    // the second word — mismatches here showed up as Windows AVs under Spring
                    // with JIT on (SportMe / insurance) while interpreter-only runs continued.
                    self.emit_xor_reg_self(RAX);
                    self.emit_store_local(field_off + 8, RAX);
                    pc += 3;
                } else {
                    self.flush_scratch_registers();
                    // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                    let (_, field_index, type_tag) =
                        match self.field_info_idx.get(&pc).map(|&i| self.field_info[i]) {
                            Some(v) => v,
                            None => {
                                if !substitute_unresolved_field_sites() {
                                    return WalkStep::Return(unresolved_field_site(pc, 0xb5));
                                }
                                (pc, 0, b'I')
                            }
                        };
                    note_field_site(&self.method_key, "putfield", pc, field_index, type_tag);
                    let receiver_mark_index = self.stack_oop_marks.len().checked_sub(2);
                    let receiver_is_trusted_oop = !self.method_key.is_empty()
                        && self.stack_oop_marks_exact
                        && receiver_mark_index
                            .and_then(|i| self.stack_oop_marks.get(i))
                            .copied()
                            .unwrap_or(false);
                    let val_slot = self.pop_stack();
                    let obj_slot = self.pop_stack();
                    // A null receiver must become a real Java NPE here, before
                    // either the inline store or the `jit_putfield_*` helper can
                    // turn it into a silent no-op. The helper's guard
                    // (`!plausible_heap_pointer(obj_ptr) { return; }`) exists to
                    // avoid dereferencing garbage, but it returns WITHOUT raising,
                    // so a `putfield` on null silently dropped the store and
                    // execution continued -- see probes/NullPutfieldProbe.java.
                    //
                    // This is the same call the inlined-callee `0xb5` arm already
                    // makes; only the top-level arm was missed when
                    // `emit_precise_null_check_field_store` landed. Inside a
                    // protected range under `precise_exception_frames` it records a
                    // reason-10 precise NPE frame; otherwise it falls back to the
                    // ordinary null-check stub. NOT emitted on the
                    // scalar-replaced branch above, whose "objectref" is a dummy
                    // with no real receiver behind it.
                    self.load_slot_to_reg(RAX, obj_slot);
                    self.emit_precise_null_check_field_store();
                    // GATED reference store — tried before every arm below,
                    // and it supersedes them on the counts that matter: it
                    // reads the collector's published barrier gates instead
                    // of inferring them from a region table G1 and ZGC
                    // leave empty (so the compact arm below is UNREACHABLE
                    // under the default collector — it emits six
                    // containment compares that cannot pass and then calls
                    // the helper), and it does not require the field's old
                    // value to be null, so an ordinary re-assignment stays
                    // inline instead of taking the helper.
                    //
                    // `false` here means "not admitted", and every arm
                    // below then runs exactly as it does today. Declining
                    // is the safe direction and the only one a missing
                    // barrier plan can produce.
                    let gated_ref_store = (type_tag == b'L' || type_tag == b'[')
                        && gated_ref_store_enabled()
                        && inline_putfield_enabled()
                        && !narrow_oops_block_inline_fields()
                        && cratonvm_types::compact_ref_fields_enabled()
                        && match self.compact_field_off.get(&pc) {
                            // `true` = the compact layout itself calls this
                            // slot a bare 8-byte reference. A `false` on an
                            // `L`/`[` field is contradictory metadata: the
                            // slot is then a 16-byte `Value` cell (the
                            // `getfield` arm's defence-in-depth reads it that
                            // way), and a bare-pointer store would overwrite
                            // its tag. Both store arms decline it to the
                            // helper, as the primitive store arm already does
                            // (r11 x64gen).
                            Some(&(c_off, true)) => {
                                // Cast: an absolute compact displacement is
                                // bounded by the object size.
                                let cell_off = c_off as i32;
                                self.emit_gated_compact_ref_putfield(
                                    obj_slot,
                                    val_slot,
                                    field_index,
                                    cell_off,
                                    receiver_is_trusted_oop,
                                )
                            }
                            _ => false,
                        };
                    if gated_ref_store {
                        // The sequence above is complete: store, both
                        // barrier gates, the helper fallback and the
                        // out-of-bounds drop all converge here.
                    } else if type_tag == b'L' || type_tag == b'[' {
                        // HIGH-5 / R20: inline the reference-field store on the
                        // barrier-free fast path (CRATONVM_JIT_INLINE_PUTFIELD).
                        // The field cell is the 16-byte `Value` enum: tag dword
                        // (`Value::Object` == 4) at FIELD_CELL_TAG_OFFSET, raw
                        // pointer payload at FIELD_CELL_PAYLOAD64_OFFSET. We emit
                        // the store directly ONLY when no GC barrier is required:
                        //   * receiver non-null;
                        //   * receiver YOUNG (`gc_flags & GC_FLAG_OLD_GEN == 0`,
                        //     header byte @7) → no generational card needed;
                        //   * field's OLD value null (payload @cell+8 == 0) → no
                        //     SATB snapshot to preserve, regardless of marking;
                        //   * index in bounds (`num_slots`, the long header's
                        //     u32 @8 -- legacy receivers only; a compact
                        //     instance has no count word).
                        // Every other case bails to `jit_putfield_object`, which
                        // performs the full SATB pre-barrier + card write-barrier.
                        // This is the fresh-init pattern (`n.left = newChild`) that
                        // dominates allocation-heavy code. Off ⇒ helper as before.
                        // Compact layout: reference fields are 8-byte
                        // pointers, so this inline 16-byte `Value` store is
                        // wrong — bail to the compact-aware
                        // `jit_putfield_object` helper.
                        if let Some(&(c_off, true)) = self.compact_field_off.get(&pc).filter(|_| {
                            // Keep compact reference stores behind the
                            // same opt-in as legacy inline putfield.
                            // A stale/misclassified compact receiver
                            // otherwise lets this bare 8-byte store
                            // scribble a Value cell during Tomcat's
                            // repeated webapp start/stop cycles.
                            inline_putfield_enabled()
                                && !narrow_oops_block_inline_fields()
                                && cratonvm_types::compact_ref_fields_enabled()
                                && self.helpers.region_bounds_addr != 0
                        }) {
                            if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_COMPACT_INLINE")
                            {
                                eprintln!("[compact-inline] putfield-ref pc={pc} off={c_off}");
                            }
                            // Reached only when the gated sequence declined
                            // (no published plan), so this arm is the
                            // pre-existing behaviour, unchanged.
                            note_ungated_ref_store();
                            // COMPACT inline reference putfield: store the
                            // bare 8-byte pointer on the barrier-free fast
                            // path (non-null YOUNG receiver, NULL old value,
                            // index in bounds), else bail to the compact-aware
                            // jit_putfield_object helper (full SATB + card).
                            // Same barrier-free reasoning as the legacy
                            // 16-byte fast path below — only the store width
                            // (8 bytes, no tag dword) and the old-value offset
                            // (cell base, not +8) differ.
                            let cell_off = c_off as i32; // Cast: absolute compact displacement, disp32
                            let mut bail: Vec<usize> = Vec::new();
                            // The baked `cell_off` is a compile-time claim
                            // about a layout the class manager can REPLACE
                            // at run time. `emit_gated_compact_ref_putfield`
                            // — the arm that ran before this one and
                            // declined — has guarded it since 2026-09-04;
                            // this ungated fallback arm did not, and every
                            // `bail` here already means "take the
                            // compact-aware helper", so it is the same
                            // vector and the same destination.
                            if jit_sp_field_layout_guard_enabled() {
                                bail.extend(self.emit_layout_epoch_guard());
                            }
                            // F-08 — the G1 arm. Under G1 the guard
                            // below rejects every receiver (empty
                            // store-side table), so this whole inline path
                            // was dead there and every reference store was
                            // an out-of-line `jit_putfield_object` call.
                            // When a G1 collector has published its
                            // geometry and `CRATONVM_G1_INLINE_BARRIER` is
                            // set, take the containment guard against the
                            // READ table (which G1 does publish, and which
                            // answers the only question the guard is doing
                            // here: can these header reads and this store
                            // fault) and emit a REAL G1 post-write barrier
                            // after the store. `region_bounds_are_live`
                            // stays false under G1 and the barrier-free
                            // premise stays unavailable — see
                            // `g1_inline_barrier_available`.
                            let g1 = self.g1_inline_barrier_available();
                            self.load_slot_to_reg(RAX, obj_slot);
                            // INT-6: null + alignment + published-region
                            // containment (subsumes the old bare null check).
                            // The YOUNG test below reads GC_FLAG_OLD_GEN,
                            // which ONLY the Generational backend maintains —
                            // under G1/ZGC every object reads as "young" and
                            // the fast path would skip G1's RSet post-barrier
                            // (edge lost, referent freed live at the next
                            // pause: Old→young after G1-1, and young→young
                            // into a JNI-pinned, CSet-excluded region even
                            // after it). G1/ZGC publish no region bounds
                            // (table all zeros), so this guard routes EVERY
                            // receiver to the full-barrier helper there;
                            // under Generational it adds the same three
                            // containment compares the guarded getfield
                            // already pays.
                            //
                            // G1-2: the trusted-oop substitution below drops
                            // exactly the containment compares that make the
                            // above true, so it is legal ONLY when the
                            // backend really has bounds published — which is
                            // the table's CONTENT, not `region_bounds_addr
                            // != 0` (the address of a process-global static,
                            // always non-zero). See `region_bounds_are_live`.
                            bail.extend(if g1 {
                                self.emit_g1_store_receiver_check()
                            } else if receiver_is_trusted_oop
                                && region_bounds_are_live(self.helpers.region_bounds_addr)
                            {
                                self.emit_trusted_oop_receiver_check()
                            } else {
                                self.emit_guarded_getfield_receiver_check(
                                    self.helpers.region_bounds_addr,
                                )
                            });
                            // LEGACY receiver (no GC_FLAG_COMPACT) → helper: the
                            // compact 8-byte cell offset is only valid for a
                            // genuinely-compact object. A class with a registered
                            // compact layout can still have uniform 16-byte-cell
                            // instances (any allocation whose `num_fields`
                            // disagrees with the layout field count — e.g.
                            // native/synthetic-stub `Method`/`ArrayList`/… whose
                            // padded stub count exceeds the real declared count).
                            // `jit_putfield_object` keys on the per-object flag
                            // and does the correct uniform-layout store. Without
                            // this the compact-offset old-value read + store would
                            // scribble a pointer into the wrong bytes of a legacy
                            // object → heap corruption / SIGSEGV.
                            self.emit_mov_r32_mem_disp32(
                                RCX,
                                RAX,
                                cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                            );
                            self.emit_and_r64_imm8(RCX, cratonvm_types::GC_FLAG_COMPACT as i8);
                            bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ not-compact → helper
                                                                        // old-gen receiver → helper (card). gc_flags @7 bit0.
                                                                        // F-08: skipped on the G1 arm — `GC_FLAG_OLD_GEN` is a
                                                                        // generational bit and a G1 rset edge is cross-REGION,
                                                                        // not old-to-young.
                            if !g1 && !self.inline_card_mark_available() {
                                self.emit_mov_r32_mem_disp32(
                                    RCX,
                                    RAX,
                                    cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                );
                                self.emit_and_r64_imm8(RCX, 1);
                                bail.push(self.emit_jcc_rel32_patch(0x85)); // JNZ old-gen
                            }
                            // g1put: under G1, only while a mark runs.
                            let no_satb = self.emit_g1_satb_gate_skip(g1);
                            // non-null OLD value → helper (SATB). The old ref
                            // is the 8-byte pointer AT the cell base.
                            self.emit_mov_r64_mem_disp32(RCX, RAX, cell_off);
                            self.emit_test_r64_r64(RCX);
                            bail.push(self.emit_jcc_rel32_patch(0x85)); // JNZ non-null old
                            if let Some(p) = no_satb {
                                self.patch_rel32_to_here(p);
                            }
                            // No field-count bounds check: the receiver was proven
                            // compact above, and a compact instance's 8-byte header
                            // has no count word (offset 8 is its first field); its
                            // layout is its class's, which declares this field.
                            // FAST STORE: bare 8-byte pointer at the cell base.
                            self.load_slot_to_reg(RDX, val_slot);
                            // gen r4w4/cards4: the inline generational
                            // barrier's PRE half (`emit_gen_card_barrier`).
                            let inline_card = !g1 && self.inline_card_mark_available();
                            // gcj j2/jt2: post-only (no PRE, no lean POST).
                            let post_only = self.post_only_card_site(inline_card);
                            // gci i23/lc: one reading for both halves; the
                            // store between them writes through RAX/RDX only.
                            let lean = inline_card
                                && !post_only
                                && super::objects::jit_lean_card_post_enabled();
                            if post_only {
                                self.note_post_only_card_site();
                            } else if inline_card {
                                self.emit_gen_card_barrier_pre(
                                    RAX,
                                    RDX,
                                    RAX,
                                    &[(RAX, obj_slot), (RDX, val_slot)],
                                    lean,
                                );
                            }
                            self.emit_mov_mem_disp32_r64(RAX, RDX, cell_off);
                            if g1 {
                                // F-08 — RCX last held the field's old value
                                // (the SATB test above; this compact arm has no
                                // num_slots bound) and is dead; RAX/RDX are
                                // clobbered by the filter and reloaded on the
                                // slow arm.
                                self.emit_g1_post_write_barrier_regs(
                                    RAX, RDX, RCX, obj_slot, val_slot,
                                );
                            } else if inline_card {
                                // ...and its POST half.
                                self.emit_gen_card_barrier_post(RAX, RDX, RAX, &[], lean);
                            }
                            let done = self.emit_jmp_rel32_patch();
                            // --- helper fallback (full barriers) ---
                            for b in bail {
                                self.patch_rel32_to_here(b);
                            }
                            self.emit_load_vm_context(ARG_REGS[0]);
                            self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                            self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast
                            self.load_slot_to_reg(ARG_REGS[3], val_slot);
                            // (vm_ptr, obj_ptr, field_index, value) -> (). The reference store is
                            // the one `putfield_*` helper that takes a `vm_ptr`, because it is the
                            // one that runs a barrier; asserting it here is what keeps that
                            // asymmetry from being re-derived by eye at each of the three arms.
                            cratonvm_jit_api::assert_helper_call_shape!(
                                "putfield_object",
                                int_args = 4,
                                returns_value = false
                            );
                            self.emit_call_absolute(self.helpers.putfield_object);
                            self.patch_rel32_to_here(done);
                        } else if inline_putfield_enabled()
                            && !cratonvm_types::compact_ref_fields_enabled()
                            && self.helpers.region_bounds_addr != 0
                        {
                            let cell_off = (HEADER_SIZE + field_index * SLOT_SIZE) as i32; // Cast: x86-64 disp32
                            let mut bail: Vec<usize> = Vec::new();
                            // F-08 — the G1 arm. Under G1 the guard
                            // below rejects every receiver (empty
                            // store-side table), so this whole inline path
                            // was dead there and every reference store was
                            // an out-of-line `jit_putfield_object` call.
                            // When a G1 collector has published its
                            // geometry and `CRATONVM_G1_INLINE_BARRIER` is
                            // set, take the containment guard against the
                            // READ table (which G1 does publish, and which
                            // answers the only question the guard is doing
                            // here: can these header reads and this store
                            // fault) and emit a REAL G1 post-write barrier
                            // after the store. `region_bounds_are_live`
                            // stays false under G1 and the barrier-free
                            // premise stays unavailable — see
                            // `g1_inline_barrier_available`.
                            let g1 = self.g1_inline_barrier_available();
                            // obj → RAX
                            self.load_slot_to_reg(RAX, obj_slot);
                            // INT-6: null + alignment + published-region
                            // containment (subsumes the old bare null check;
                            // null still reaches the helper, matching its
                            // no-op semantics). The YOUNG test below reads
                            // GC_FLAG_OLD_GEN, which ONLY the Generational
                            // backend maintains — under G1/ZGC every object
                            // reads as "young" and this fast path would elide
                            // G1's RSet post-barrier (Old→young before G1-1;
                            // young→young into a JNI-pinned, CSet-excluded
                            // region after it). G1/ZGC publish no region
                            // bounds (table all zeros), so every receiver
                            // bails to the full-barrier helper there.
                            //
                            // G1-2: same reasoning as the compact arm above —
                            // the trusted-oop substitution removes the
                            // containment compares, so it is conditional on
                            // the bounds table actually holding live bounds.
                            bail.extend(if g1 {
                                self.emit_g1_store_receiver_check()
                            } else if receiver_is_trusted_oop
                                && region_bounds_are_live(self.helpers.region_bounds_addr)
                            {
                                self.emit_trusted_oop_receiver_check()
                            } else {
                                self.emit_guarded_getfield_receiver_check(
                                    self.helpers.region_bounds_addr,
                                )
                            });
                            // old-gen receiver → helper (card barrier). gc_flags is
                            // the exported gc_flags byte; GC_FLAG_OLD_GEN == bit 0.
                            // F-08: not on the G1 arm; see the compact twin.
                            if !g1 && !self.inline_card_mark_available() {
                                self.emit_mov_r32_mem_disp32(
                                    RCX,
                                    RAX,
                                    cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                );
                                self.emit_and_r64_imm8(RCX, 1);
                                bail.push(self.emit_jcc_rel32_patch(0x85)); // JNZ old-gen
                            }
                            // g1put: under G1, only while a mark runs.
                            let no_satb = self.emit_g1_satb_gate_skip(g1);
                            // non-null OLD value → helper (SATB). Read the cell's
                            // 8-byte payload; a null old value never needs SATB.
                            self.emit_mov_r64_mem_disp32(
                                RCX,
                                RAX,
                                cell_off + FIELD_CELL_PAYLOAD64_OFFSET as i32, // Cast: layout offset → disp32
                            );
                            self.emit_test_r64_r64(RCX);
                            bail.push(self.emit_jcc_rel32_patch(0x85)); // JNZ non-null old
                            if let Some(p) = no_satb {
                                self.patch_rel32_to_here(p);
                            }
                            // bounds: field_index < num_slots (long-header u32 @8).
                            // 32-bit compare — an 8-byte read would fold in the
                            // adjacent identity-hash word (@12).
                            self.emit_mov_r32_mem_disp32(
                                RCX,
                                RAX,
                                cratonvm_types::NUM_SLOTS_OFFSET as i32,
                            );
                            self.emit_mov_imm64(RDX, field_index as i64); // RDX = field_index
                            self.emit_cmp_r32_r32(RDX, RCX); // cmp field_index, num_slots
                            let oob = self.emit_jcc_rel32_patch(0x83); // JAE → out of bounds, drop
                                                                       // gen r4w4/cards4: the inline generational
                                                                       // barrier's PRE half, before EITHER word of the
                                                                       // cell is written (`emit_gen_card_barrier`).
                            let inline_card = !g1 && self.inline_card_mark_available();
                            // gcj j2/jt2: post-only (no PRE, no lean POST); the
                            // payload store below reloads RDX itself.
                            let post_only = self.post_only_card_site(inline_card);
                            // gci i23/lc: one reading for both halves; the
                            // two cell stores between them use RCX/RDX only.
                            let lean = inline_card
                                && !post_only
                                && super::objects::jit_lean_card_post_enabled();
                            if post_only {
                                self.note_post_only_card_site();
                            } else if inline_card {
                                self.load_slot_to_reg(RDX, val_slot);
                                self.emit_gen_card_barrier_pre(
                                    RAX,
                                    RDX,
                                    RAX,
                                    &[(RAX, obj_slot), (RDX, val_slot)],
                                    lean,
                                );
                            }
                            // FAST STORE. tag dword = 4 (+ zeroed pad dword) via a
                            // sign-extended imm32 qword store; payload = value.
                            self.emit_mov_imm64(RCX, 4);
                            self.emit_mov_mem_disp32_r64(
                                RAX,
                                RCX,
                                cell_off + FIELD_CELL_TAG_OFFSET as i32, // Cast: layout offset → disp32
                            );
                            self.load_slot_to_reg(RDX, val_slot);
                            self.emit_mov_mem_disp32_r64(
                                RAX,
                                RDX,
                                cell_off + FIELD_CELL_PAYLOAD64_OFFSET as i32, // Cast: layout offset → disp32
                            );
                            if g1 {
                                // F-08 — RCX last held the num_slots bound
                                // (and, above, the tag immediate) and is
                                // dead here.
                                self.emit_g1_post_write_barrier_regs(
                                    RAX, RDX, RCX, obj_slot, val_slot,
                                );
                            } else if inline_card {
                                // POST half of the inline generational barrier.
                                self.emit_gen_card_barrier_post(RAX, RDX, RAX, &[], lean);
                            }
                            let done = self.emit_jmp_rel32_patch();
                            // --- helper fallback (full barriers) ---
                            for b in bail {
                                self.patch_rel32_to_here(b);
                            }
                            self.emit_load_vm_context(ARG_REGS[0]);
                            self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                            self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                            self.load_slot_to_reg(ARG_REGS[3], val_slot);
                            // (vm_ptr, obj_ptr, field_index, value) -> (). The reference store is
                            // the one `putfield_*` helper that takes a `vm_ptr`, because it is the
                            // one that runs a barrier; asserting it here is what keeps that
                            // asymmetry from being re-derived by eye at each of the three arms.
                            cratonvm_jit_api::assert_helper_call_shape!(
                                "putfield_object",
                                int_args = 4,
                                returns_value = false
                            );
                            self.emit_call_absolute(self.helpers.putfield_object);
                            // join: the out-of-bounds skip and the post-store jump
                            // both land here (after the helper).
                            self.patch_rel32_to_here(oob);
                            self.patch_rel32_to_here(done);
                        } else {
                            self.emit_load_vm_context(ARG_REGS[0]);
                            self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                            self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                            self.load_slot_to_reg(ARG_REGS[3], val_slot);
                            // (vm_ptr, obj_ptr, field_index, value) -> (). The reference store is
                            // the one `putfield_*` helper that takes a `vm_ptr`, because it is the
                            // one that runs a barrier; asserting it here is what keeps that
                            // asymmetry from being re-derived by eye at each of the three arms.
                            cratonvm_jit_api::assert_helper_call_shape!(
                                "putfield_object",
                                int_args = 4,
                                returns_value = false
                            );
                            self.emit_call_absolute(self.helpers.putfield_object);
                        }
                    } else if self.try_emit_inline_primitive_putfield(
                        self.compact_field_off.get(&pc).copied(),
                        obj_slot,
                        val_slot,
                        field_index,
                        type_tag,
                        receiver_is_trusted_oop,
                    ) {
                        // Complete: the inline store, and the helper call
                        // below on every path it declined at run time.
                    } else {
                        self.load_slot_to_reg(ARG_REGS[0], obj_slot);
                        self.emit_mov_imm32_sx(ARG_REGS[1], field_index as i32); // Cast: x86-64 immediate encoding
                        self.load_slot_to_reg(ARG_REGS[2], val_slot);
                        // `jit_putfield_int` stores `Value::Int` whole into a
                        // legacy cell; narrow a sub-int field's value here, as
                        // the interpreter does (r9-ea). No-op for javac output.
                        self.emit_narrow_to_field_tag(ARG_REGS[2], type_tag);
                        let helper = match type_tag {
                            b'J' => self.helpers.putfield_long,
                            b'F' => self.helpers.putfield_float,
                            b'D' => self.helpers.putfield_double,
                            _ => self.helpers.putfield_int,
                        };
                        // The primitive stores take (obj_ptr, field_index, value) with NO
                        // `vm_ptr` — three registers, not four — and return nothing. One
                        // sequence serves four slots; all four are asserted.
                        cratonvm_jit_api::assert_helper_call_shape!(
                            "putfield_int",
                            int_args = 3,
                            returns_value = false
                        );
                        cratonvm_jit_api::assert_helper_call_shape!(
                            "putfield_long",
                            int_args = 3,
                            returns_value = false
                        );
                        cratonvm_jit_api::assert_helper_call_shape!(
                            "putfield_float",
                            int_args = 3,
                            returns_value = false
                        );
                        cratonvm_jit_api::assert_helper_call_shape!(
                            "putfield_double",
                            int_args = 3,
                            returns_value = false
                        );
                        self.emit_call_absolute(helper);
                    }
                    pc += 3;
                }
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

/// `CRATONVM_JIT_INLINE_PRIM_PUTFIELD=1` (`CRATONVM_JIT=inline-prim-putfield`) —
/// store primitive `putfield`s inline instead of calling `jit_putfield_*`.
///
/// Default ON since round 9 wave 4's integration; `0` is the kill switch. It
/// was OFF while A/B'd (it writes the heap, and a wrong cell layout is silent
/// corruption rather than a crash) -- and the A/B found exactly that: the value
/// was loaded after the compact/legacy branch, so the legacy arm stored the
/// field index. With that fixed, on the wave-4 binary: executed tests for all
/// eight primitive types on both layouts; the regression suite 95/95 and the
/// JIT probes identical to HotSpot with it forced on; `PrimStoreProbe` top-level
/// stores 1 368 -> 277 ms, setter 1 469 -> 847, counters 762 -> 240. See
/// [`Compiler::try_emit_inline_primitive_putfield`] for the contract and
/// `docs/known-issues/jit/single-pass-primitive-putfield-is-always-a-helper-call-20260918.md`
/// for why it exists. NOT `OnceLock`-cached, for the reason
/// `guarded_inline_getfield_enabled` gives: a compile-time gate read once per
/// call site, where caching would only make the switch racy against the first
/// compile in the process.
pub(super) fn inline_primitive_putfield_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_INLINE_PRIM_PUTFIELD")
}

/// `CRATONVM_JIT_SP_SPLICED_STORE_TRUSTED_RECEIVER` — round 16 lane alloc,
/// proposal OT-3 (`jit-r15w5-osrtier-proposals-RETIRED-20261003.md`): a `putfield` inside a
/// spliced body whose receiver is provably the splice's local 0
/// ([`spliced_putfield_receiver_is_local0`]) takes the trusted-oop receiver
/// guard (one null test) instead of the null / alignment / three-range heap
/// containment sequence (~14 instructions), in both the primitive store
/// ([`Compiler::try_emit_inline_primitive_putfield`]) and the gated reference
/// store (`emit_gated_compact_ref_putfield`).
///
/// The proof is the one the spliced `getfield` arm has used by default since
/// round 11 (`local0_is_trusted_oop` in `try_emit_inline_body`): local 0 is
/// bound from an operand the caller's exact type tracker marked an oop, the
/// callee never writes local 0, and the splice is the outermost one. The
/// typical site is a spliced constructor's `this.f = arg`, whose receiver is
/// the object the caller's `new` just allocated -- the allocate-and-construct
/// loop pays four such checks per object. No barrier changes: the gated store
/// still reads every published gate, and a primitive store has none. Valid
/// under every collector: the containment check is a "can this address
/// fault" question (the READ table), never a barrier decision.
///
/// Default ON since round 16 (measured: single-pass alloc-store
/// 18.2→16.9 ns, alloc-ref-dead 14.7→13.5 ns); `0`/`false`/`off`/`no` turns it
/// off. NOT `OnceLock`-cached, for the reason
/// [`inline_primitive_putfield_enabled`] gives.
pub(super) fn sp_spliced_store_trusted_receiver_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_SP_SPLICED_STORE_TRUSTED_RECEIVER")
}

/// Is the receiver of the `putfield` at `cpc` in a spliced body exactly the
/// value the instruction two before it pushed from local 0?
///
/// True only for the shape `aload_0 | aload 0`, then ONE single-slot push
/// (`aconst_null`, `iconst_*`, `lconst_*`, `fconst_*`, `dconst_*`, `bipush`,
/// `sipush`, `xload n`, `xload_n`), then the `putfield` -- `this.f = arg` and
/// `this.f = 0`. Neither the push nor the `putfield` may be a branch target:
/// at a join the textually preceding instructions need not be the ones that
/// ran (the rule `emit_trusted_oop_receiver_check_at` states for `getfield`).
/// With both reached only by fall-through, the operand under the value is the
/// one `aload 0` pushed.
///
/// Instruction starts are found by a forward walk from 0, so a `cpc` that is
/// not an instruction start, a `wide`, or any undecodable byte answers
/// `false`. Whether local 0 holds a trusted oop is the caller's half
/// (`local0_is_trusted_oop`).
pub(super) fn spliced_putfield_receiver_is_local0(
    code: &[u8],
    code_len: usize,
    cpc: usize,
    branch_targets: &[bool],
) -> bool {
    let code = &code[..code_len.min(code.len())];
    if code.get(cpc) != Some(&0xb5) {
        return false;
    }
    let (mut prev, mut last) = (None::<usize>, None::<usize>);
    let mut pc = 0usize;
    while pc < cpc {
        if code[pc] == 0xc4 {
            return false; // wide
        }
        let Some(len) = crate::bytecode_analysis::insn_len(code, pc) else {
            return false;
        };
        prev = last;
        last = Some(pc);
        pc += len;
    }
    let (Some(recv), Some(val)) = (prev, last) else {
        return false;
    };
    if pc != cpc
        || branch_targets.get(val).is_none_or(|&t| t)
        || branch_targets.get(cpc).is_none_or(|&t| t)
    {
        return false;
    }
    let recv_is_local0 = match code[recv] {
        0x2a => recv + 1 == val,
        0x19 => code.get(recv + 1) == Some(&0) && recv + 2 == val,
        _ => false,
    };
    let val_len = match code[val] {
        // aconst_null, iconst_m1..5, lconst_0/1, fconst_0..2, dconst_0/1.
        0x01..=0x0f => 1,
        // bipush.
        0x10 => 2,
        // sipush.
        0x11 => 3,
        // iload, lload, fload, dload, aload <n>.
        0x15..=0x19 => 2,
        // xload_0..xload_3.
        0x1a..=0x2d => 1,
        _ => return false,
    };
    recv_is_local0 && val + val_len == cpc
}

/// The `Value` discriminant a legacy 16-byte field cell carries for each
/// primitive store width, i.e. word 0's low dword as
/// `cratonvm_types::value_words` writes it (`Int = 0`, `Long = 1`,
/// `Float = 2`, `Double = 3`). Sub-int fields are `Value::Int`. Pinned against
/// `value_words` by `r9w2_prim_putfield_tests`.
pub(super) fn legacy_cell_tag_for(type_tag: u8) -> u32 {
    match type_tag {
        b'J' => 1,
        b'F' => 2,
        b'D' => 3,
        _ => 0,
    }
}

impl Compiler {
    /// Inline primitive `putfield` (`I J F D Z B C S`) — the store twin of the
    /// guarded inline `getfield`, behind [`inline_primitive_putfield_enabled`].
    ///
    /// Returns `false`, having emitted NOTHING, when it does not apply; the
    /// caller then emits the ordinary `jit_putfield_*` call. Returns `true`
    /// when it emitted a complete sequence, whose every declined run-time path
    /// ends in that same helper call.
    ///
    /// Entry: the receiver has passed `emit_precise_null_check_field_store`
    /// (a null receiver already threw), the scratch cache is flushed, and
    /// both operands are in `obj_slot` / `val_slot`.
    ///
    /// What the helper does, reproduced (`vm/src/jit/helpers.rs`,
    /// `jit_putfield_{int,long,float,double}`):
    ///
    /// 1. an implausible receiver is not dereferenced — here the guarded
    ///    containment check against the READ bounds table (or the null test
    ///    for a proven oop), failing to the helper;
    /// 2. `field_index < num_slots`, else the store is dropped — here failing
    ///    to the helper, which drops it and records why;
    /// 3. a compact object (`GC_FLAG_COMPACT`) stores the field's own width at
    ///    the layout's absolute displacement `c_off` (`write_compact_field`);
    ///    a legacy object gets a whole 16-byte `Value` cell, word 0 (tag + 32-bit payload) then word 1
    ///    (64-bit payload or zero), in `write_value_atomic`'s order;
    /// 4. a sub-int value is narrowed first (`emit_narrow_to_field_tag`), as the
    ///    `jit_putfield_int` arm does.
    ///
    /// A compact receiver at a site with no baked compact offset, a replaced
    /// layout (the epoch guard), and anything the gates do not cover all take
    /// the helper. No barrier: a primitive store has none.
    ///
    /// `compact` is this site's `(compact byte offset, is_reference)` when it
    /// has one: `compact_field_off[pc]` at the top level, the callee's
    /// `InlineSite::compact_field_info` row in a splice (r9w3, x64obj3).
    pub(super) fn try_emit_inline_primitive_putfield(
        &mut self,
        compact: Option<(u32, bool)>,
        obj_slot: StackSlot,
        val_slot: StackSlot,
        field_index: usize,
        type_tag: u8,
        receiver_is_trusted_oop: bool,
    ) -> bool {
        if !inline_primitive_putfield_enabled()
            || !inline_putfield_enabled()
            || narrow_oops_block_inline_fields()
            || self.helpers.read_bounds_addr == 0
            || !matches!(
                type_tag,
                b'I' | b'J' | b'F' | b'D' | b'Z' | b'B' | b'C' | b'S'
            )
        {
            return false;
        }
        let compact_on = cratonvm_types::compact_ref_fields_enabled();
        // The compact cell, when this site has one. A compact offset that
        // claims a REFERENCE for a primitive descriptor is contradictory
        // metadata: decline and let the helper resolve it.
        let compact_off: Option<i32> = if compact_on {
            match compact {
                Some((_, true)) => return false,
                Some((c_off, false)) => {
                    let Some(off) = i32::try_from(c_off).ok() else {
                        return false;
                    };
                    Some(off)
                }
                None => None,
            }
        } else {
            None
        };
        // The legacy 16-byte cell: word 0 at `legacy_off`, word 1 at `+ 8`.
        let Some(legacy_off) = field_index
            .checked_mul(SLOT_SIZE)
            .and_then(|b| b.checked_add(HEADER_SIZE))
            .and_then(|o| i32::try_from(o).ok())
        else {
            return false;
        };
        // Cast: FIELD_CELL_PAYLOAD64_OFFSET is 8, a layout constant.
        let payload64 = FIELD_CELL_PAYLOAD64_OFFSET as i32;
        let Some(legacy_hi_off) = legacy_off.checked_add(payload64) else {
            return false;
        };
        let Ok(field_index_imm) = i64::try_from(field_index) else {
            return false;
        };

        let mut slow: Vec<usize> = Vec::new();
        // A baked compact offset is a claim about a layout the class manager
        // can replace at run time; every emitter that bakes one guards it.
        // Before the receiver load: the fallback form clobbers R11 and RCX.
        if compact_off.is_some() && jit_sp_field_layout_guard_enabled() {
            slow.extend(self.emit_layout_epoch_guard());
        }
        self.load_slot_to_reg(RAX, obj_slot);
        // Receiver: null / unaligned / outside every published heap region →
        // helper. Clobbers RCX and RDX, never RAX.
        slow.extend(if receiver_is_trusted_oop {
            self.emit_trusted_oop_receiver_check()
        } else {
            self.emit_guarded_getfield_receiver_check(self.helpers.read_bounds_addr)
        });
        // Bounds: field_index < num_slots, else helper -- for a LEGACY
        // (long-header) receiver only, whose field count sits at
        // `NUM_SLOTS_OFFSET`. A compact instance's 8-byte header has no count
        // word; when compact objects can exist the check moves into the legacy
        // arm below, after the per-object layout test.
        if !compact_on {
            self.emit_mov_r32_mem_disp32(RCX, RAX, cratonvm_types::NUM_SLOTS_OFFSET as i32); // Cast: layout offset → disp32
            self.emit_mov_imm64(RDX, field_index_imm);
            self.emit_cmp_r32_r32(RDX, RCX);
            slow.push(self.emit_jcc_rel32_patch(0x83)); // JAE → helper
        }
        // The value, narrowed to a sub-int field's width. BEFORE the layout
        // branch: both arms store RDX, and the layout test below touches only
        // RCX. (It sat after the `JZ -> legacy cell` until round 9's
        // integration, so the legacy arm stored the field index the bounds
        // check had left in RDX.)
        self.load_slot_to_reg(RDX, val_slot);
        self.emit_narrow_to_field_tag(RDX, type_tag);
        // Per-object layout, exactly as the guarded getfield decides it.
        let legacy_patch = if compact_on {
            self.emit_mov_r32_mem_disp32(RCX, RAX, cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32); // Cast: layout offset → disp32
            self.emit_and_r64_imm8(RCX, cratonvm_types::GC_FLAG_COMPACT as i8); // Cast: flag bit fits imm8
            Some(self.emit_jcc_rel32_patch(0x84)) // JZ → legacy cell
        } else {
            None
        };
        let mut done: Vec<usize> = Vec::new();
        if let Some(legacy_patch) = legacy_patch {
            // --- compact object ---
            match compact_off {
                Some(off) => {
                    self.emit_store_rdx_width(off, type_tag);
                    done.push(self.emit_jmp_rel32_patch());
                }
                // A compact receiver at a site with no compact offset: the
                // helper resolves the packed offset.
                None => slow.push(self.emit_jmp_rel32_patch()),
            }
            self.patch_rel32_to_here(legacy_patch);
            // The legacy receiver's bounds check (see above). RDX holds the
            // value, so the field index goes through R11.
            self.emit_mov_r32_mem_disp32(RCX, RAX, cratonvm_types::NUM_SLOTS_OFFSET as i32); // Cast: layout offset → disp32
            self.emit_mov_imm64(R11, field_index_imm);
            self.emit_cmp_r32_r32(R11, RCX);
            slow.push(self.emit_jcc_rel32_patch(0x83)); // JAE → helper
        }
        // --- legacy object: a whole `Value` cell, word 0 then word 1 ---
        let tag = legacy_cell_tag_for(type_tag);
        if matches!(type_tag, b'J' | b'D') {
            // word 0 = tag (bytes 4..8 zero); word 1 = the 64-bit payload.
            self.buf.emit(&[0x48, 0xC7, 0x80]); // MOV qword [RAX + disp32], imm32
            self.buf.emit(&legacy_off.to_le_bytes());
            self.buf.emit(&tag.to_le_bytes());
            self.emit_mov_mem_disp32_r64(RAX, RDX, legacy_hi_off);
        } else {
            // word 0 = tag | payload32 << 32; word 1 = 0.
            self.buf.emit(&[0x89, 0xD1]); // MOV ECX, EDX (zero-extends)
            self.buf.emit(&[0x48, 0xC1, 0xE1, 0x20]); // SHL RCX, 32
            if tag != 0 {
                // Cast: tag is 0..=3, fits imm8.
                self.buf.emit(&[0x48, 0x83, 0xC9, tag as u8]); // OR RCX, imm8
            }
            self.emit_mov_mem_disp32_r64(RAX, RCX, legacy_off);
            self.buf.emit(&[0x31, 0xC9]); // XOR ECX, ECX
            self.emit_mov_mem_disp32_r64(RAX, RCX, legacy_hi_off);
        }
        done.push(self.emit_jmp_rel32_patch());

        // --- helper fallback: the unchanged `jit_putfield_*` call ---
        for p in slow {
            self.patch_rel32_to_here(p);
        }
        self.load_slot_to_reg(ARG_REGS[0], obj_slot);
        self.emit_mov_imm32_sx(ARG_REGS[1], field_index as i32); // Cast: x86-64 immediate encoding (checked above via legacy_off)
        self.load_slot_to_reg(ARG_REGS[2], val_slot);
        self.emit_narrow_to_field_tag(ARG_REGS[2], type_tag);
        let helper = match type_tag {
            b'J' => self.helpers.putfield_long,
            b'F' => self.helpers.putfield_float,
            b'D' => self.helpers.putfield_double,
            _ => self.helpers.putfield_int,
        };
        cratonvm_jit_api::assert_helper_call_shape!(
            "putfield_int",
            int_args = 3,
            returns_value = false
        );
        cratonvm_jit_api::assert_helper_call_shape!(
            "putfield_long",
            int_args = 3,
            returns_value = false
        );
        cratonvm_jit_api::assert_helper_call_shape!(
            "putfield_float",
            int_args = 3,
            returns_value = false
        );
        cratonvm_jit_api::assert_helper_call_shape!(
            "putfield_double",
            int_args = 3,
            returns_value = false
        );
        self.emit_call_absolute(helper);
        for p in done {
            self.patch_rel32_to_here(p);
        }
        true
    }

    /// `MOV [RAX + disp32], {DL | DX | EDX | RDX}` — the compact cell store
    /// of a primitive field of descriptor `type_tag`, at its own width
    /// (`write_compact_field`: `Z`/`B` one byte, `C`/`S` two, `I`/`F` four,
    /// `J`/`D` eight).
    fn emit_store_rdx_width(&mut self, disp: i32, type_tag: u8) {
        match type_tag {
            b'Z' | b'B' => self.buf.emit(&[0x88, 0x90]), // MOV byte [RAX+disp32], DL
            b'C' | b'S' => self.buf.emit(&[0x66, 0x89, 0x90]), // MOV word [RAX+disp32], DX
            b'J' | b'D' => {
                self.emit_mov_mem_disp32_r64(RAX, RDX, disp);
                return;
            }
            _ => self.buf.emit(&[0x89, 0x90]), // MOV dword [RAX+disp32], EDX
        }
        self.buf.emit(&disp.to_le_bytes());
    }

    /// Round 16 (lane fieldguard): a PRIMITIVE `putstatic` as a direct store
    /// into the class's statics cell -- the store twin of
    /// `try_emit_inline_getstatic`, behind `CRATONVM_JIT_INLINE_PUTSTATIC`
    /// (default ON since round 16; `crate::ir_lower::inline_putstatic_enabled`, shared with
    /// the optimizing tier's `Lowerer::emit_inline_putstatic`, whose doc
    /// carries the argument).
    ///
    /// In short: `resolve_static_base` answers only for an initialized class
    /// with a published block covering `field_index` (and never for
    /// `java/lang/System`), so the helper has no `<clinit>` to run, no block to
    /// create and -- for a primitive, whose cell never holds a reference -- no
    /// SATB pre-barrier to fire. What is left is its write of the 16-byte
    /// `Value` cell, reproduced here word for word (the inline primitive
    /// `putfield`'s legacy-cell words: word 0 = `tag | payload32 << 32`,
    /// word 1 = 0, or word 0 = `tag`, word 1 = payload for `J`/`D`). The base
    /// pointer is re-read from the baked cell on every execution, as the
    /// inline `getstatic` re-reads it.
    ///
    /// ```text
    ///   MOV RDX, value ; narrowed to the field's width
    ///   MOV RAX, imm64 ; &statics_index[class].base  (the POINTER cell)
    ///   MOV RAX, [RAX] ; the class's statics block
    ///   MOV [RAX + field_index*16], word0 ; MOV [RAX + field_index*16 + 8], word1
    /// ```
    ///
    /// Clobbers RAX, RCX and RDX; the caller has flushed the operand-stack
    /// register cache. The volatile StoreLoad fence stays the caller's.
    /// Returns `false`, having emitted nothing, when the site is not admitted.
    pub(super) fn try_emit_inline_putstatic(
        &mut self,
        class_id_raw: u32,
        field_index: usize,
        type_tag: u8,
        val_slot: StackSlot,
    ) -> bool {
        if !crate::ir_lower::inline_putstatic_enabled() {
            return false;
        }
        let wide = matches!(type_tag, b'J' | b'D');
        if !wide && !matches!(type_tag, b'I' | b'Z' | b'B' | b'C' | b'S' | b'F') {
            return false;
        }
        let Some(base_cell) = self
            .direct_helpers
            .resolve_static_base(class_id_raw, field_index)
        else {
            return false;
        };
        // Cast: cell byte offset within the class's statics block -> disp32.
        let Ok(cell_off) = i32::try_from(field_index.saturating_mul(SLOT_SIZE)) else {
            return false;
        };
        // Cast: fixed layout offset to an i32 instruction displacement.
        let Some(hi_off) = cell_off.checked_add(FIELD_CELL_PAYLOAD64_OFFSET as i32) else {
            return false;
        };
        let tag = legacy_cell_tag_for(type_tag);
        // The value, narrowed exactly as the helper route narrows it
        // (JVMS §6.5 `putstatic`; a no-op for every tag but Z/B/C/S).
        self.load_slot_to_reg(RDX, val_slot);
        self.emit_narrow_to_field_tag(RDX, type_tag);
        // Cast: the baked address of the never-freed base-pointer cell.
        self.emit_mov_imm64(RAX, base_cell as i64);
        self.emit_mov_r64_mem_disp32(RAX, RAX, 0);
        if wide {
            // word 0 = tag (bytes 4..8 zero); word 1 = the 64-bit payload.
            self.buf.emit(&[0x48, 0xC7, 0x80]); // MOV qword [RAX + disp32], imm32
            self.buf.emit(&cell_off.to_le_bytes());
            self.buf.emit(&tag.to_le_bytes());
            self.emit_mov_mem_disp32_r64(RAX, RDX, hi_off);
        } else {
            // word 0 = tag | payload32 << 32; word 1 = 0.
            self.buf.emit(&[0x89, 0xD1]); // MOV ECX, EDX (zero-extends)
            self.buf.emit(&[0x48, 0xC1, 0xE1, 0x20]); // SHL RCX, 32
            if tag != 0 {
                // Cast: tag is 0..=3, fits imm8.
                self.buf.emit(&[0x48, 0x83, 0xC9, tag as u8]); // OR RCX, imm8
            }
            self.emit_mov_mem_disp32_r64(RAX, RCX, cell_off);
            self.buf.emit(&[0x31, 0xC9]); // XOR ECX, ECX
            self.emit_mov_mem_disp32_r64(RAX, RCX, hi_off);
        }
        true
    }

    /// Narrow the int in `reg` to the width of a sub-int field, exactly as the
    /// interpreter's `narrow_int_to_field_type` does on every `putfield` (and as
    /// JVMS §6.5 / HotSpot do): `B` sign-extends the low byte, `S` the low 16
    /// bits, `C` zero-extends the low 16 bits, `Z` keeps bit 0. Any other tag
    /// emits nothing.
    ///
    /// javac narrows before every such store (`i2b`, `i2s`, `i2c`, a boolean is
    /// already 0/1), so for its bytecode this is one register instruction that
    /// changes nothing; it is what makes a store from any other producer read
    /// back the same in compiled code as in the interpreter (r9-ea, see
    /// `docs/known-issues/jit/sub-int-field-stores-are-not-narrowed-by-compiled-code-20260918.md`).
    pub(super) fn emit_narrow_to_field_tag(&mut self, reg: u8, type_tag: u8) {
        let hi = reg >= 8;
        let rm = reg & 7;
        // mod = 11, reg field = rm field = the register itself.
        let modrm = 0xC0 | (rm << 3) | rm;
        match type_tag {
            b'B' | b'S' => {
                // MOVSX r64, r/m8 (0F BE) / r/m16 (0F BF). REX.W always, so a
                // byte source in 4..=7 is SPL..DIL, never AH..BH; REX.R and
                // REX.B together name a high register on both sides.
                let rex = 0x48 | if hi { 0x05 } else { 0x00 };
                let op2 = if type_tag == b'B' { 0xBE } else { 0xBF };
                self.buf.emit(&[rex, 0x0F, op2, modrm]);
            }
            b'C' => {
                // MOVZX r32, r/m16 (0F B7); a 32-bit destination zero-extends.
                if hi {
                    self.buf.emit_byte(0x45); // REX.R | REX.B
                }
                self.buf.emit(&[0x0F, 0xB7, modrm]);
            }
            b'Z' => {
                // AND r/m32, imm8 (83 /4 ib); zero-extends the upper half.
                if hi {
                    self.buf.emit_byte(0x41); // REX.B
                }
                self.buf.emit(&[0x83, 0xE0 | rm, 0x01]);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod r9w2_prim_putfield_tests {
    use super::*;
    use cratonvm_types::Value;

    /// The two words `write_value_atomic` — the helper's legacy-cell writer —
    /// leaves in a 16-byte cell for `v`.
    fn cell_of(v: Value) -> [u64; 2] {
        let mut cell = [0xAAAA_AAAA_AAAA_AAAAu64; 2];
        // SAFETY: a 16-byte, 8-aligned buffer is exactly one `Value` cell.
        unsafe { cratonvm_types::write_value_atomic(cell.as_mut_ptr() as *mut Value, v) };
        cell
    }

    /// The inline legacy store writes word 0 = `tag | payload32 << 32`, word
    /// 1 = 0 for a narrow value and word 0 = `tag`, word 1 = payload for a wide
    /// one. Those are the helper's words exactly, or a compiled store and an
    /// interpreted one leave different cells behind.
    #[test]
    fn legacy_cell_words_match_the_helper_s() {
        let w = cell_of(Value::Int(-5));
        assert_eq!(w[0] & 0xFFFF_FFFF, u64::from(legacy_cell_tag_for(b'I')));
        assert_eq!(w[0] >> 32, 0xFFFF_FFFB, "the Int payload is the low dword");
        assert_eq!(w[1], 0, "word 1 of a narrow value is zero");

        let w = cell_of(Value::Float(1.5));
        assert_eq!(
            w[0],
            u64::from(legacy_cell_tag_for(b'F')) | (u64::from(1.5f32.to_bits()) << 32)
        );
        assert_eq!(w[1], 0);

        let w = cell_of(Value::Long(-7));
        assert_eq!(
            w,
            [u64::from(legacy_cell_tag_for(b'J')), 0xFFFF_FFFF_FFFF_FFF9]
        );
        let w = cell_of(Value::Double(2.5));
        assert_eq!(w, [u64::from(legacy_cell_tag_for(b'D')), 2.5f64.to_bits()]);

        for t in [b'Z', b'B', b'C', b'S'] {
            assert_eq!(
                legacy_cell_tag_for(t),
                legacy_cell_tag_for(b'I'),
                "a sub-int field's cell is a `Value::Int`"
            );
        }
    }

    /// The compact store writes the field's OWN width
    /// (`write_compact_field`): one byte for `Z`/`B`, two for `C`/`S`, four
    /// for `I`/`F`, eight for `J`/`D` — never a wider store that would clobber
    /// the neighbouring packed field.
    #[test]
    fn compact_stores_have_the_field_s_own_width() {
        let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        let cases: [(u8, &[u8]); 8] = [
            (b'Z', &[0x88, 0x90]),
            (b'B', &[0x88, 0x90]),
            (b'C', &[0x66, 0x89, 0x90]),
            (b'S', &[0x66, 0x89, 0x90]),
            (b'I', &[0x89, 0x90]),
            (b'F', &[0x89, 0x90]),
            (b'J', &[0x48, 0x89, 0x90]),
            (b'D', &[0x48, 0x89, 0x90]),
        ];
        for (tag, opcode) in cases {
            let start = c.buf.pos();
            c.emit_store_rdx_width(0x1234, tag);
            let got: Vec<u8> = c.buf.as_slice()[start..c.buf.pos()].to_vec();
            let mut want: Vec<u8> = opcode.to_vec();
            want.extend_from_slice(&0x1234i32.to_le_bytes());
            assert_eq!(got, want, "compact store of a `{}` field", tag as char);
        }
    }

    /// Default ON since round 9 wave 4; `0` turns it off.
    #[test]
    fn the_inline_primitive_store_is_default_on_with_a_kill_switch() {
        let _unset = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                "CRATONVM_JIT_INLINE_PRIM_PUTFIELD",
                None,
            )]),
        );
        assert!(inline_primitive_putfield_enabled());
        drop(_unset);
        let _off = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                "CRATONVM_JIT_INLINE_PRIM_PUTFIELD",
                Some("0"),
            )]),
        );
        assert!(!inline_primitive_putfield_enabled());
    }

    /// Round 16 (lane fieldguard, FG-b3): the single-pass layout guard (one
    /// emitter, `Compiler::emit_layout_epoch_guard`, for top-level and
    /// inlined-callee field sites alike) bakes the epoch the compilation began
    /// at, so a layout replacement between row resolution and emission fails
    /// it.
    #[test]
    fn the_single_pass_layout_guard_bakes_the_compiles_starting_epoch() {
        fn baked(code: &[u8]) -> u32 {
            let imm = |at: usize| {
                u32::from_le_bytes([code[at], code[at + 1], code[at + 2], code[at + 3]])
            };
            if code.len() >= 10 && code[0] == 0x81 && code[1] == 0x3D {
                return imm(6);
            }
            let at = code
                .windows(2)
                .position(|w| w == [0x81, 0xF9])
                .expect("an epoch compare was emitted");
            imm(at + 2)
        }
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_LAYOUT_EPOCH_SNAPSHOT", Some("1"))],
            || {
                let witness = crate::open_compile_epoch_witness();
                let snap = crate::compile_layout_epoch_snapshot().expect("the witness snapshots");
                // A REPLACEMENT of a registered layout bumps the epoch.
                let cid = 900_017u32;
                let mk = |body: u32| {
                    std::sync::Arc::new(cratonvm_types::CompactLayout {
                        field_disps: vec![8],
                        is_ref: vec![false],
                        field_kinds: vec![cratonvm_types::FieldStorageKind::Int],
                        ref_disps: Vec::new(),
                        total_size: body,
                    })
                };
                cratonvm_types::register_class_layout(
                    cratonvm_types::FIRST_LAYOUT_DOMAIN,
                    cid,
                    mk(4),
                );
                cratonvm_types::register_class_layout(
                    cratonvm_types::FIRST_LAYOUT_DOMAIN,
                    cid,
                    mk(8),
                );
                assert!(cratonvm_types::layout_replace_epoch() > snap);
                let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
                let from = c.buf.pos();
                assert!(c.emit_layout_epoch_guard().is_some());
                assert_eq!(
                    baked(&c.buf.as_slice()[from..]),
                    snap,
                    "the guard bakes the compile's starting epoch"
                );
                drop(witness);
            },
        );
    }
}

/// Round 16 lane alloc (OT-3): the spliced-store receiver proof and the
/// trusted-receiver primitive store, executed.
#[cfg(all(test, target_arch = "x86_64"))]
mod r16_alloc_spliced_store_tests {
    use super::*;

    fn local0(code: &[u8], cpc: usize) -> bool {
        let targets = crate::bytecode_analysis::branch_target_map(code, code.len());
        spliced_putfield_receiver_is_local0(code, code.len(), cpc, &targets)
    }

    #[test]
    fn the_receiver_proof_admits_this_dot_f_equals_one_push() {
        // aload_0; iload_1; putfield #2; return
        assert!(local0(&[0x2a, 0x1b, 0xb5, 0, 2, 0xb1], 2));
        // aload 0; bipush 7; putfield
        assert!(local0(&[0x19, 0, 0x10, 7, 0xb5, 0, 2, 0xb1], 4));
        // aload_0; sipush; putfield
        assert!(local0(&[0x2a, 0x11, 1, 2, 0xb5, 0, 2, 0xb1], 4));
        // aload_0; aconst_null; putfield, after an earlier store.
        let code = [0x2a, 0x1b, 0xb5, 0, 2, 0x2a, 0x01, 0xb5, 0, 3, 0xb1];
        assert!(local0(&code, 2));
        assert!(local0(&code, 7));
    }

    #[test]
    fn the_receiver_proof_refuses_everything_else() {
        // aload_1 is not local 0.
        assert!(!local0(&[0x2b, 0x1b, 0xb5, 0, 2, 0xb1], 2));
        // aload 1 (wide-less form) is not local 0.
        assert!(!local0(&[0x19, 1, 0x1b, 0xb5, 0, 2, 0xb1], 3));
        // Two-instruction value: aload_0; iload_1; iconst_1; iadd; putfield.
        assert!(!local0(&[0x2a, 0x1b, 0x04, 0x60, 0xb5, 0, 2, 0xb1], 4));
        // ldc is not admitted as the value push.
        assert!(!local0(&[0x2a, 0x12, 1, 0xb5, 0, 2, 0xb1], 3));
        // Not a putfield at cpc.
        assert!(!local0(&[0x2a, 0x1b, 0xb4, 0, 2, 0xb1], 2));
        // cpc not an instruction start.
        assert!(!local0(&[0x2a, 0x1b, 0xb5, 0, 2, 0xb1], 3));
        // The value push is a branch target:
        //   0: iload_1; 1: ifeq +5 (-> 6); 4: aload_0; 5: aload_2;
        //   6: iload_1 (target); 7: putfield
        // (an ill-typed join, but the textual rule must refuse it anyway).
        let code = [0x1b, 0x99, 0, 5, 0x2a, 0x2c, 0x1b, 0xb5, 0, 2, 0xb1];
        assert!(!local0(&code, 7));
        // The putfield is a branch target: 0: aload_0; 1: iload_1;
        // 2: goto +3 (-> 5); 5: putfield.
        let code = [0x2a, 0x1b, 0xa7, 0, 3, 0xb5, 0, 2, 0xb1];
        assert!(!local0(&code, 5));
        // A `wide` before the site refuses.
        let code = [0xc4, 0x15, 0, 1, 0x2a, 0x1b, 0xb5, 0, 2, 0xb1];
        assert!(!local0(&code, 6));
    }

    #[test]
    fn the_switch_is_default_on() {
        // Default ON since round 16; `0` is the kill switch.
        let _unset = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                "CRATONVM_JIT_SP_SPLICED_STORE_TRUSTED_RECEIVER",
                None,
            )]),
        );
        assert!(sp_spliced_store_trusted_receiver_enabled());
        drop(_unset);
        let _off = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                "CRATONVM_JIT_SP_SPLICED_STORE_TRUSTED_RECEIVER",
                Some("0"),
            )]),
        );
        assert!(!sp_spliced_store_trusted_receiver_enabled());
    }

    /// Word 3 of the fake object: where the helper stub records that it ran
    /// (no process-global state: `jit/tests/process_global_statics_ratchet.rs`).
    const HELPER_MARK_WORD: usize = 3;

    // SAFETY: extern "C" test stub; `obj` is null or the test's live 32-byte
    // fake object, whose word 3 is outside the stored field.
    unsafe extern "C" fn stub_putfield_int(obj: i64, _idx: i64, val: i64) {
        if obj != 0 {
            *(obj as *mut u64).add(HELPER_MARK_WORD) = 1000 + val as u64;
        }
    }

    /// Emit `try_emit_inline_primitive_putfield` (compact `I` field at byte 8,
    /// receiver in frame slot 8, value in slot 16) and run it once. Returns
    /// whether the emitter applied.
    fn run(trusted: bool, obj: i64, val: i64) -> bool {
        // A READ bounds table that contains nothing: the containment check
        // rejects every receiver, the trusted guard does not consult it.
        let empty_bounds = [0usize; 6];
        let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        c.helpers.read_bounds_addr = empty_bounds.as_ptr() as usize; // Cast: table address
        c.helpers.putfield_int = stub_putfield_int as *const () as usize; // Cast: fn address
        let entry_off = c.buf.pos();
        c.buf.emit(&[0x55]); // push rbp
        c.buf.emit(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
        c.buf.emit(&[0x48, 0x83, 0xEC, 0x40]); // sub rsp, 0x40
        c.emit_store_local(8, ARG_REGS[0]);
        c.emit_store_local(16, ARG_REGS[1]);
        let applied = c.try_emit_inline_primitive_putfield(
            Some((8, false)),
            StackSlot::Frame(8),
            StackSlot::Frame(16),
            0,
            b'I',
            trusted,
        );
        c.buf.emit(&[0xC9, 0xC3]); // leave; ret
        assert!(!c.buf.overflowed());
        if !applied {
            return false;
        }
        crate::platform::make_executable(c.buf.as_ptr() as *mut u8, c.buf.capacity())
            .expect("the test buffer must be flippable to RX");
        // SAFETY: `entry_off` is inside the same allocation.
        let entry = unsafe { c.buf.as_ptr().add(entry_off) };
        // SAFETY: the snippet builds its own frame, writes only through `obj`
        // (a live test buffer or null, which the guard sends to the stub), and
        // calls only the stub above. `c` outlives the call.
        let f: extern "C" fn(i64, i64) = unsafe { std::mem::transmute(entry) };
        f(obj, val);
        drop(c);
        true
    }

    /// The trusted guard stores a compact receiver inline where the
    /// containment guard (against a table that contains nothing) sends the
    /// same store to the helper; a null receiver still reaches the helper.
    #[test]
    fn a_trusted_receiver_stores_inline_without_the_containment_check() {
        if !cratonvm_types::compact_ref_fields_enabled() {
            return; // no compact layout in this environment
        }
        let mut obj = [0u64; 4];
        // SAFETY: byte offset inside the 32-byte buffer.
        unsafe {
            *(obj.as_mut_ptr() as *mut u8).add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) =
                cratonvm_types::GC_FLAG_COMPACT;
        }
        let p = obj.as_mut_ptr() as i64;

        if !run(false, p, 41) {
            return; // the inline store is switched off in this environment
        }
        assert_eq!(obj[HELPER_MARK_WORD], 1041, "untrusted: the helper ran");
        assert_eq!(obj[1] as u32, 0, "untrusted: nothing stored inline");

        // The site bakes the global layout-replacement epoch; a concurrent test
        // that re-registers a class layout bumps it between emission and the
        // call, and the epoch guard then (correctly) sends the store to the
        // helper. Not this test's question: retry while the epoch moves.
        let mut stable = false;
        for _ in 0..8 {
            obj[HELPER_MARK_WORD] = 0;
            obj[1] = 0;
            let epoch = cratonvm_types::layout_replace_epoch();
            assert!(run(true, p, 42));
            if cratonvm_types::layout_replace_epoch() == epoch {
                stable = true;
                break;
            }
        }
        if !stable {
            return; // the epoch never held still across one run
        }
        assert_eq!(obj[HELPER_MARK_WORD], 0, "trusted: no helper");
        assert_eq!(obj[1] as u32, 42, "trusted: the cell holds the value");

        // A null receiver must reach the helper (which ignores it here); a
        // missing null test would fault on the header read instead.
        assert!(run(true, 0, 43));
    }
}
