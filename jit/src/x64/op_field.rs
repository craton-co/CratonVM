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
    #[allow(clippy::too_many_arguments)]
    pub(super) fn walk_field(
        &mut self,
        code: &[u8],
        _code_len: usize,
        op: u8,
        mut pc: usize,
        _dead: &mut bool,
        _branch_targets: &[bool],
    ) -> WalkStep {
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
                            return WalkStep::Return( unresolved_field_site(pc, 0xb2));
                        }
                        (pc, 0, 0, b'I', false)
                    }
                };

                // Direct load, no helper CALL — the structural fix this
                // opcode's long bail comment above describes. It emits the
                // value push, the oop mark and the volatile fence itself,
                // so the whole helper sequence below is skipped.
                if self.try_emit_inline_getstatic(
                    class_id_raw,
                    field_index,
                    type_tag,
                    is_volatile,
                ) {
                    pc += 3;
                    return WalkStep::Next(pc);
                }

                self.flush_scratch_registers();
                self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
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
                // Volatile static: emit MFENCE after read (SeqCst acquire)
                if is_volatile {
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
            0xb3 => {
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
                            return WalkStep::Return( unresolved_field_site(pc, 0xb3));
                        }
                        (pc, 0, 0, b'I', false)
                    }
                };

                let val_slot = self.pop_stack();

                // Select the appropriate helper based on type_tag
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
                // Call jit_putstatic_xxx(vm_ptr, class_id_raw, field_index, val)
                self.emit_load_local(ARG_REGS[0], self.heap_local_offset); // vm_ptr
                self.emit_mov_imm32_sx(ARG_REGS[1], class_id_raw as i32); // class_id // Cast: x86-64 immediate encoding
                self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // field_index // Cast: x86-64 immediate encoding
                self.load_slot_to_reg(ARG_REGS[3], val_slot); // value
                                                              // `<clinit>` on first touch, as for `getstatic`: a safepoint.
                self.emit_pre_safepoint_spill();
                self.emit_call_absolute(helper_fn);
                self.emit_oop_map_for_safepoint();
                // jit-putstatic-clinit-gap fix (2026-07-17): see the
                // matching comment at the inlined-callee 0xb3 arm above —
                // same helper, same new fallible-`<clinit>` sentinel.
                self.emit_post_invoke_exception_check(b'V');
                // Volatile static: emit MFENCE after write (SeqCst store-load barrier)
                if is_volatile {
                    self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
                }
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
                                    return WalkStep::Return( unresolved_field_site(pc, 0xb4));
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
                        eprintln!(
                            "[compact-inline] getfield pc={pc} off={c_off} ref={c_is_ref}"
                        );
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
                    // per-object `GC_FLAG_COMPACT` header bit (byte 21) the
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
                                    return WalkStep::Return( unresolved_field_site(pc, 0xb4));
                                }
                                (pc, 0, b'I')
                            }
                        };
                    let cell_off = (HEADER_SIZE + c_off as usize) as i32; // Cast: x86-64 disp32
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
                    let trusted_top_is_oop =
                        self.stack_oop_marks.last().copied().unwrap_or(false);
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
                            "[compact-inline] getfield pc={pc} NOT-trusted-oop                                  have_key={trusted_have_key} marks_exact={trusted_marks_exact}                                  top_is_oop={trusted_top_is_oop} depth={} method={}",
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
                            self.emit_trusted_oop_receiver_check_at(code, pc, true, 0),
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
                    // Per-object compactness: gc_flags byte @21 & GC_FLAG_COMPACT.
                    // Zero ⇒ legacy 16-byte-cell object → uniform-layout read.
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
                            b'C' => {
                                self.emit_movx_r64_mem_disp32(RAX, RAX, cell_off, 16, false)
                            }
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
                        self.buf.emit(
                            &(legacy_cell_off + FIELD_CELL_TAG_OFFSET as i32).to_le_bytes(),
                        );
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
                        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                        self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                        self.emit_getfield_index_arg(ARG_REGS[2], field_index, type_tag, pc);
                        crate::metrics::note_getfield_arm(1);
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
                        (inline_getfield_enabled()
                            && !cratonvm_types::compact_ref_fields_enabled())
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
                    let raw_mode = inline_getfield_enabled()
                        && !cratonvm_types::compact_ref_fields_enabled();
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
                            // its `GC_FLAGS` read at `[RAX + 15]`, which is
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
                        // valid for a legacy-laid-out object. gc_flags byte @21.
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
                        self.bind_implicit_null_recovery();
                        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                        self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                        self.emit_getfield_index_arg(ARG_REGS[2], field_index, type_tag, pc);
                        crate::metrics::note_getfield_arm(2);
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
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                    self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                    self.emit_getfield_index_arg(ARG_REGS[2], field_index, type_tag, pc);
                    crate::metrics::note_getfield_arm(3);
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
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
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
                    let (_, field_index, _type_tag) =
                        match self.field_info_idx.get(&pc).map(|&i| self.field_info[i]) {
                            Some(v) => v,
                            None => {
                                if !substitute_unresolved_field_sites() {
                                    return WalkStep::Return( unresolved_field_site(pc, 0xb5));
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
                                    return WalkStep::Return( unresolved_field_site(pc, 0xb5));
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
                            Some(&(c_off, _)) => {
                                // Cast: a compact field offset plus the
                                // header is bounded by the object size.
                                let cell_off = (HEADER_SIZE + c_off as usize) as i32;
                                self.emit_gated_compact_ref_putfield(
                                    obj_slot,
                                    val_slot,
                                    field_index,
                                    cell_off,
                                    receiver_is_trusted_oop,
                                )
                            }
                            None => false,
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
                        //     header byte @21) → no generational card needed;
                        //   * field's OLD value null (payload @cell+8 == 0) → no
                        //     SATB snapshot to preserve, regardless of marking;
                        //   * index in bounds (`num_slots`, header u32 @16).
                        // Every other case bails to `jit_putfield_object`, which
                        // performs the full SATB pre-barrier + card write-barrier.
                        // This is the fresh-init pattern (`n.left = newChild`) that
                        // dominates allocation-heavy code. Off ⇒ helper as before.
                        // Compact layout: reference fields are 8-byte
                        // pointers, so this inline 16-byte `Value` store is
                        // wrong — bail to the compact-aware
                        // `jit_putfield_object` helper.
                        if let Some(&(c_off, _)) =
                            self.compact_field_off.get(&pc).filter(|_| {
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
                            })
                        {
                            if cratonvm_types::flags::runtime_flag_on(
                                "CRATONVM_DBG_COMPACT_INLINE",
                            )
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
                            let cell_off = (HEADER_SIZE + c_off as usize) as i32; // Cast: disp32
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
                                                                        // old-gen receiver → helper (card). gc_flags @21 bit0.
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
                            // non-null OLD value → helper (SATB). The old ref
                            // is the 8-byte pointer AT the cell base.
                            self.emit_mov_r64_mem_disp32(RCX, RAX, cell_off);
                            self.emit_test_r64_r64(RCX);
                            bail.push(self.emit_jcc_rel32_patch(0x85)); // JNZ non-null old
                                                                        // bounds: field_index < num_slots (header u32 @12).
                            self.emit_mov_r32_mem_disp32(
                                RCX,
                                RAX,
                                cratonvm_types::NUM_SLOTS_OFFSET as i32,
                            );
                            self.emit_mov_imm64(RDX, field_index as i64);
                            self.emit_cmp_r32_r32(RDX, RCX);
                            let oob = self.emit_jcc_rel32_patch(0x83); // JAE → drop
                                                                       // FAST STORE: bare 8-byte pointer at the cell base.
                            self.load_slot_to_reg(RDX, val_slot);
                            self.emit_mov_mem_disp32_r64(RAX, RDX, cell_off);
                            if g1 {
                                // F-08 — RCX last held the num_slots bound
                                // and is dead; RAX/RDX are clobbered by the
                                // filter and reloaded on the slow arm.
                                self.emit_g1_post_write_barrier_regs(
                                    RAX, RDX, RCX, obj_slot, val_slot,
                                );
                            } else if self.inline_card_mark_available() {
                                self.emit_inline_card_mark_regs(RAX, RDX);
                            }
                            let done = self.emit_jmp_rel32_patch();
                            // --- helper fallback (full barriers) ---
                            for b in bail {
                                self.patch_rel32_to_here(b);
                            }
                            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                            self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                            self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast
                            self.load_slot_to_reg(ARG_REGS[3], val_slot);
                            self.emit_call_absolute(self.helpers.putfield_object);
                            self.patch_rel32_to_here(oob);
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
                            // non-null OLD value → helper (SATB). Read the cell's
                            // 8-byte payload; a null old value never needs SATB.
                            self.emit_mov_r64_mem_disp32(
                                RCX,
                                RAX,
                                cell_off + FIELD_CELL_PAYLOAD64_OFFSET as i32, // Cast: layout offset → disp32
                            );
                            self.emit_test_r64_r64(RCX);
                            bail.push(self.emit_jcc_rel32_patch(0x85)); // JNZ non-null old
                                                                        // bounds: field_index < num_slots (header u32 @12).
                                                                        // 32-bit compare — an 8-byte read would fold in the
                                                                        // adjacent gc_age/gc_flags bytes.
                            self.emit_mov_r32_mem_disp32(
                                RCX,
                                RAX,
                                cratonvm_types::NUM_SLOTS_OFFSET as i32,
                            );
                            self.emit_mov_imm64(RDX, field_index as i64); // RDX = field_index
                            self.emit_cmp_r32_r32(RDX, RCX); // cmp field_index, num_slots
                            let oob = self.emit_jcc_rel32_patch(0x83); // JAE → out of bounds, drop
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
                            } else if self.inline_card_mark_available() {
                                self.emit_inline_card_mark_regs(RAX, RDX);
                            }
                            let done = self.emit_jmp_rel32_patch();
                            // --- helper fallback (full barriers) ---
                            for b in bail {
                                self.patch_rel32_to_here(b);
                            }
                            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                            self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                            self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                            self.load_slot_to_reg(ARG_REGS[3], val_slot);
                            self.emit_call_absolute(self.helpers.putfield_object);
                            // join: the out-of-bounds skip and the post-store jump
                            // both land here (after the helper).
                            self.patch_rel32_to_here(oob);
                            self.patch_rel32_to_here(done);
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
