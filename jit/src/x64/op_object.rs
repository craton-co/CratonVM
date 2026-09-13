// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Allocation, type checks and monitors: `new`, `newarray`, `anewarray`, `multianewarray`, `checkcast`, `instanceof`, `monitorenter` and `monitorexit` in the single-pass backend's bytecode walk.
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
    pub(super) fn walk_object(
        &mut self,
        code: &[u8],
        _code_len: usize,
        op: u8,
        mut pc: usize,
        _dead: &mut bool,
        _branch_targets: &[bool],
    ) -> WalkStep {
        match op {
            // newarray — allocate a new primitive array
            0xbc => {
                self.flush_scratch_registers();
                let atype = code[pc + 1] as i32; // Widening: always safe
                let count_slot = self.pop_stack();
                // Round-8 wave-3: defensive callee-saved spill
                // before any GC-triggering CALL.
                //
                // Emitted BEFORE the fast path, not between it and the
                // helper, because both arms merge into one oop map below
                // and the spill has to cover the edge that calls. The
                // inline arm never calls, so it pays stores it does not
                // need — the `new` arm's `sink_alloc_blind_spill` is the
                // machinery for withholding them, and wiring an array-
                // shaped request into it is a separate change from giving
                // arrays a bump at all.
                self.emit_pre_safepoint_spill();
                // The inline TLAB bump, with `helpers.newarray` as its own
                // slow path. Declines (and emits nothing) for a shape it
                // cannot serve, leaving the unconditional call below.
                //
                // `ArrayElementType`'s discriminants ARE the JVM `atype`
                // values (`Boolean = 4` … `Long = 11`), which is what makes
                // `array_element_type_from_tag` the decode here; an
                // unrecognised tag yields `None` and keeps the call, the
                // same outcome `jit_newarray`'s own `_ => return 0` arm has.
                // Cast: an `atype` is one bytecode operand byte.
                let inlined = cratonvm_types::array_element_type_from_tag(atype as u8)
                    .map(|elem| self.emit_inline_tlab_newarray(elem, atype, count_slot))
                    .unwrap_or(false);
                if !inlined {
                    // Load heap pointer → ARG_REGS[0]
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                    // atype immediate → ARG_REGS[1]
                    self.emit_mov_imm32_sx(ARG_REGS[1], atype);
                    // count → ARG_REGS[2]
                    self.load_slot_to_reg(ARG_REGS[2], count_slot);
                    self.emit_call_absolute(self.helpers.newarray);
                }
                // T1.1.a — `newarray` is a GC-triggering safepoint.
                self.emit_oop_map_for_safepoint();
                // Heap-exhaustion guard: a null result means OOM (the helper
                // stashed an OutOfMemoryError). Bail before the null is pushed
                // and dereferenced by a following `arraylength`/store.
                self.emit_post_alloc_oom_check();
                self.push_from_rax();
                // A primitive array header is still an object reference.
                self.mark_top_as_oop();
                pc += 2;
            }

            // new — allocate a new Java object (heap allocation via helper)
            // Scalar replacement: if the object doesn't escape, store fields
            // in the JIT frame instead of heap-allocating.
            0xbb => {
                if let Some(sr_obj) = self.scalar_replaced.get(&pc).cloned() {
                    // Scalar-replaced: zero-initialize field slots in the frame
                    self.emit_xor_reg_self(RAX);
                    for i in 0..sr_obj.num_fields {
                        let field_off = sr_obj.field_base_offset + (i as i32) * (SLOT_SIZE as i32); // Cast: x86-64 immediate encoding
                        self.emit_store_local(field_off, RAX);
                        self.emit_store_local(field_off + 8, RAX);
                    }
                    // Push a dummy zero "object reference" — never dereferenced
                    self.push_from_rax();
                    pc += 3;
                } else {
                    self.flush_scratch_registers();
                    // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                    let resolved = self.new_info_idx.get(&pc).map(|&i| self.new_info[i]);
                    let (_, class_id_raw, num_fields, has_prim_init, has_finalizer) = match resolved
                    {
                        Some(info) => info,
                        None => {
                            // Not compile-time resolvable: the target
                            // class was not loaded when this method was
                            // compiled (the cold `throw new
                            // SomeException(...)` shape). Emit the
                            // CP-indexed helper, which resolves +
                            // initialises + allocates at run time, the
                            // way the interpreter's 0xbb handler does.
                            // No compile-time class knowledge exists
                            // here, so neither the inline TLAB bump nor
                            // scalar replacement applies — this site is
                            // always the helper call, which is exactly
                            // right for a branch that is (by hypothesis)
                            // cold.
                            let Some(&i) = self.new_deferred_idx.get(&pc) else {
                                return WalkStep::Return(false);
                            };
                            let (_, holder_class_id, cp_idx) = self.new_deferred_info[i];
                            if self.helpers.new_object_cp == 0 || !self.needs_heap {
                                return WalkStep::Return(false);
                            }
                            self.emit_pre_safepoint_spill();
                            crate::runtime_lowering::emit_new_object_cp_stub(
                                &mut self.buf,
                                self.heap_local_offset,
                                self.helpers.new_object_cp,
                                holder_class_id,
                                cp_idx,
                                self.helpers.frame_record,
                            );
                            // Same post-call contract as the resolved
                            // arm below: GC-triggering safepoint, then
                            // the 0/null sentinel guard (the helper
                            // reports a failed class resolution,
                            // `<clinit>` failure or OOM by stashing a
                            // pending exception and returning 0).
                            self.emit_oop_map_for_safepoint();
                            self.emit_post_alloc_oom_check();
                            self.push_from_rax();
                            self.mark_top_as_oop();
                            pc += 3;
                            return WalkStep::Next(pc);
                        }
                    };

                    // HIGH-6 JIT audit (object_allocation/1000 3-5x gap):
                    // emit an inline TLAB bump-pointer fast path when the
                    // helper table is fully wired AND the class is small
                    // enough to bump in a single LEA disp32 (< 256 bytes
                    // total, which covers HashMap.Node, ArrayList$Itr,
                    // and ~99% of common allocation sites). Larger
                    // objects and the test-helper path (no `get_current_thread`
                    // wired) fall through to the unconditional helper call.
                    //
                    // The inline path bumps `thread.tlab.cursor`, writes
                    // `class_id` at obj_ptr+0, then tail-calls
                    // `jit_post_tlab_init` to finish header + primitive
                    // defaults + finalizer registration. The bump itself
                    // is ~6 instructions; HotSpot achieves ~5-7. The
                    // remaining work (hash mint, num_slots write, class-
                    // metadata RwLock for primitive defaults) is in the
                    // post-init helper — kept out of inline because
                    // synthesising it would require per-field descriptor
                    // plumbing that isn't currently in `new_info`.
                    let total_size = HEADER_SIZE + num_fields * SLOT_SIZE;
                    // `total_size` here is the legacy upper bound used only
                    // for the <=256 inline-eligibility gate; the compact body
                    // is smaller, so a legacy fit implies a compact fit.
                    // `emit_inline_tlab_new` computes the real compact size +
                    // writes array_length/GC_FLAG_COMPACT inline.
                    // The pure inline path publishes a complete canonical
                    // header before advancing the TLAB cursor and cannot
                    // call into GC. Enable that safe subset by default.
                    // Body clearing makes int-family typed zeroes safe in
                    // the pure inline path. Sites requiring non-zero Value
                    // tags (long/float/double) or finalizer registration
                    // retain the helper path unless explicitly opted in.
                    //
                    // (Restored 2026-07-14: merge 96a1d0c2 resolved this
                    // region to the pre-0ee32e122 opt-in gate — reverting
                    // "Optimize Binary Trees allocation and recursion" and
                    // regressing bt18 1.9s→6.0s. The old "not yet safe
                    // with the precise moving young collector" rationale
                    // belonged to the pre-redesign inline path; the
                    // current one completes the header before the cursor
                    // advance, which is what made default-on safe.)
                    // Whether the post-init helper would have anything to
                    // do: this is the INLINE-ELIGIBILITY question, and it
                    // is about the class alone.
                    let helper_is_noop = !has_prim_init && !has_finalizer;
                    // Whether we may actually drop the call. A collector
                    // whose sweep is driven by an allocation-base registry
                    // rather than by walking the chunk (ZGC) has to be told
                    // about every object, and this helper is the only place
                    // an inline allocation can tell it -- an unannounced
                    // object is not an object to `is_object_address`, and
                    // its first use as a receiver decodes as `null`. So the
                    // call stays, and only the bump is inlined.
                    let skip_helper =
                        helper_is_noop && !cratonvm_types::jit_tlab_registration_required();
                    let can_inline =
                        !cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_DISABLE_INLINE_NEW")
                            && (helper_is_noop
                                || cratonvm_types::flags::runtime_flag_on(
                                    "CRATONVM_JIT_ENABLE_INLINE_NEW",
                                ))
                            && self.helpers.get_current_thread != 0
                            && self.helpers.tlab_post_init != 0
                            && self.helpers.new_object != 0
                            && total_size <= 256
                            && self.needs_heap; // need vm_ptr in heap_local slot

                    // Round-8 wave-3: defensive callee-saved spill
                    // before the `new` safepoint (both inline TLAB
                    // and slow-path helper can reach GC via
                    // jit_post_tlab_init / new_object).
                    //
                    // ...but only the paths that CALL one of those can. With
                    // `skip_helper` the inline arm emits no call at all
                    // between here and the merge point, so no collector can
                    // observe this frame on it, and the blind full-GPR half
                    // of the spill is dead there. Ask for it to be sunk onto
                    // the allocation's slow-path edges instead; the request
                    // degrades to the full spill if the safepoint emitter
                    // cannot honour it. `inline_tlab_new_enabled()` is part
                    // of the condition because its opt-out turns the inline
                    // arm back into a bare `new_object` call.
                    self.sink_alloc_blind_spill = alloc_spill_sink_enabled()
                        && can_inline
                        && skip_helper
                        && inline_tlab_new_enabled()
                        && self.jit_thread_slot_off != 0;
                    self.emit_pre_safepoint_spill();
                    if can_inline {
                        // CRIT-2 — when neither primitive-init nor
                        // finalizer registration is required, skip
                        // the `jit_post_tlab_init` helper and write
                        // identity_hash/num_slots inline. Most JDK
                        // micro-objects (HashMap.Node, ArrayList$Itr,
                        // Iterator chains, all-reference field
                        // bearers) hit this fast path. The
                        // resolution of these flags currently
                        // requires extending `cp_new_resolver` (see
                        // the `new_info` doc in `jit/src/lib.rs`),
                        // so the conservative default `(true,true)`
                        // keeps the helper call in place for now.
                        self.emit_inline_tlab_new(class_id_raw, num_fields, skip_helper);
                    } else {
                        // Slow path: full helper-call dispatch. Used when
                        //   - the helper table is partial (tests),
                        //   - the object exceeds 256 bytes (rare —
                        //     HotSpot also bails on these),
                        //   - or the method's prologue did not stash
                        //     `vm_ptr` in a frame slot.
                        crate::runtime_lowering::emit_new_object_stub(
                            &mut self.buf,
                            self.heap_local_offset,
                            self.helpers.new_object,
                            class_id_raw,
                            num_fields,
                            self.helpers.frame_record,
                        );
                    }
                    // The withheld half of the blind spill must have been
                    // emitted by now — the only consumer is the arm above.
                    // Fail the compile closed rather than ship a safepoint
                    // whose spill is missing eleven registers: an unspilled
                    // register-resident oop is a reclaimed live object, and
                    // the cost of bailing is one interpreted method.
                    if self.deferred_alloc_blind_spill {
                        self.deferred_alloc_blind_spill = false;
                        self.fail("singlepass-codegen/alloc-spill-sink-unconsumed");
                    }
                    // T1.1.a — `new` is a GC-triggering safepoint.
                    // Emit an oop map for the slots that were live
                    // BEFORE the call (the return value hasn't
                    // been pushed yet, so the stack state here
                    // reflects the surviving operands). Both arms
                    // (inline and slow) may trigger GC: the inline
                    // path's post-init helper can grow the
                    // finalizer queue and the slow path obviously
                    // can young-GC.
                    self.emit_oop_map_for_safepoint();
                    // Heap-exhaustion guard: both the inline-TLAB slow path
                    // and the slow-path helper return the 0/null sentinel on
                    // OOM (jit_new_object -> jit_alloc_oom). Bail before the
                    // null is pushed and dereferenced by a following
                    // getfield/putfield. (Scalar-replaced `new` never reaches
                    // here, so its dummy-zero push is unaffected.)
                    self.emit_post_alloc_oom_check();
                    self.push_from_rax();
                    // The result is an object reference.
                    self.mark_top_as_oop();
                    pc += 3;
                }
            }

            // anewarray — allocate a new reference array via helper
            0xbd => {
                self.flush_scratch_registers();
                // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                let resolved = self
                    .anewarray_info_idx
                    .get(&pc)
                    .map(|&i| self.anewarray_info[i]);
                let (_, component_class_id_raw) = match resolved {
                    Some(info) => info,
                    None => {
                        // Component class not loaded at compile time — the
                        // `anewarray` sibling of the deferred `new` arm
                        // above. Emit the CP-indexed helper, which resolves
                        // the component class at run time and then does
                        // exactly what `jit_anewarray_object` does.
                        let Some(&i) = self.anewarray_deferred_idx.get(&pc) else {
                            return WalkStep::Return(false); // genuinely unresolvable — bail
                        };
                        let (_, holder_class_id, cp_idx) = self.anewarray_deferred_info[i];
                        if self.helpers.anewarray_object_cp == 0 || !self.needs_heap {
                            return WalkStep::Return(false);
                        }
                        let count_slot = self.pop_stack();
                        // jit_anewarray_object_cp(vm, holder_class_id, cp_idx, length)
                        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                        self.emit_mov_imm32_sx(ARG_REGS[1], holder_class_id as i32); // Cast: x86-64 immediate encoding
                        self.emit_mov_imm32_sx(ARG_REGS[2], cp_idx as i32); // Cast: x86-64 immediate encoding
                        self.load_slot_to_reg(ARG_REGS[3], count_slot);
                        self.emit_pre_safepoint_spill();
                        self.emit_call_absolute(self.helpers.anewarray_object_cp);
                        // Resolution can run a user `ClassLoader.loadClass`,
                        // i.e. arbitrary Java on this thread — republish the
                        // frame afterwards exactly as `emit_new_object_stub`
                        // does for the `new` side.
                        crate::runtime_lowering::emit_post_call_frame_republish(
                            &mut self.buf,
                            self.helpers.frame_record,
                        );
                        self.emit_oop_map_for_safepoint();
                        self.emit_post_alloc_oom_check();
                        self.push_from_rax();
                        self.mark_top_as_oop();
                        pc += 3;
                        return WalkStep::Next(pc);
                    }
                };
                let count_slot = self.pop_stack();
                // jit_anewarray_object(heap, component_class_id_raw, length) → i64 array ptr
                self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                self.emit_mov_imm32_sx(ARG_REGS[1], component_class_id_raw as i32); // Cast: x86-64 immediate encoding
                self.load_slot_to_reg(ARG_REGS[2], count_slot);
                // Round-8 wave-3: defensive callee-saved spill
                // before any GC-triggering CALL.
                self.emit_pre_safepoint_spill();
                self.emit_call_absolute(self.helpers.anewarray_object);
                // T1.1.a — `anewarray` is a GC-triggering safepoint.
                self.emit_oop_map_for_safepoint();
                // Heap-exhaustion / negative-length guard: jit_anewarray_object
                // returns the 0/null sentinel on OOM (-> jit_alloc_oom) or on
                // a negative length (-> jit_negative_array_size). Bail before
                // the null is pushed and dereferenced.
                self.emit_post_alloc_oom_check();
                self.push_from_rax();
                // The result is a reference array — an object reference.
                self.mark_top_as_oop();
                pc += 3;
            }

            // checkcast (0xc0) — type check (pass-through or exception)
            0xc0 => {
                self.flush_scratch_registers();
                // Look up resolved typecheck info for this PC
                // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                let (_, name_ptr, name_len) = self
                    .typecheck_info_idx
                    .get(&pc)
                    .map(|&i| self.typecheck_info[i])
                    .unwrap_or((pc, std::ptr::null(), 0));
                // A site with no resolved target cannot be compiled
                // correctly: `jit_checkcast` has no class to test against
                // and answers with a silent null, which turns an object
                // into `null` instead of passing it or throwing. Stay
                // interpreted. The height is unchanged (pop one, push one),
                // so the model stays plausible until the post-loop check.
                if name_ptr.is_null() || name_len == 0 {
                    self.fail("singlepass-codegen/checkcast-unresolved-site");
                    pc += 3;
                    return WalkStep::Next(pc);
                }

                // ---- inline class-id fast path (see `checkcast_inline_enabled`) ----
                //
                // The target `ClassId` needs NO new plumbing: the main
                // compile door already resolves it and interns the site's
                // name under that identity, precisely so the runtime helper
                // can compare ids instead of re-resolving a name, and
                // `typecheck_target_for_site` is the public read of that
                // table. The id and the name pointer baked into the helper
                // call below are therefore the SAME pair — they cannot
                // disagree about which class this site means.
                //
                // A `ClassId` is per-VM and that table is process-wide, but
                // `intern_typecheck_target` keys its intern on the id, so
                // two VMs resolving one name differently get two DISTINCT
                // pointers and two distinct rows. The `JitCache` is a
                // per-VM field (`vm_init`), so a body only ever runs in the
                // VM whose compile baked the immediate.
                //
                // The doors that do not intern (OSR, eager first-call) get
                // `None` here and keep the unconditional call. That is a
                // real coverage gap and it is COUNTED rather than assumed —
                // see `CHECKCAST_INLINE_NO_TARGET_ID`.
                let target_class_id = if name_ptr.is_null() {
                    None
                } else {
                    crate::typecheck_target_for_site(name_ptr)
                }
                .filter(|&id| id != 0);

                // Read BEFORE the pop: `stack_oop_marks` is parallel to
                // `stack`, so the operand's mark is the last one only while
                // the operand is still on it. Same three clauses, read the
                // same way, as the compact inline `getfield` arm above —
                // whose fast path raw-dereferences these same references at
                // a FIELD offset behind a bare null test, which is why
                // reading the class id at offset 0 asks nothing new of them.
                let trusted_have_key = !self.method_key.is_empty();
                let trusted_marks_exact = self.stack_oop_marks_exact;
                let trusted_top_is_oop = self.stack_oop_marks.last().copied().unwrap_or(false);
                let operand_is_trusted_oop =
                    trusted_have_key && trusted_marks_exact && trusted_top_is_oop;

                let obj_slot = self.pop_stack();

                // The 1-D primitive-array variant. It needs NO class id —
                // it proves its answer from the header's kind/element tags —
                // which is the whole point, because a primitive array's
                // class id is 0 and could never have matched.
                // SAFETY: `name_ptr`/`name_len` are an `intern_typecheck_target`
                // pair, leaked for the life of the process.
                let prim_array_tag = unsafe { crate::typecheck_site_name(name_ptr, name_len) }
                    .and_then(cratonvm_types::primitive_array_kind_tags_byte)
                    .filter(|_| checkcast_inline_enabled() && operand_is_trusted_oop);
                let inline_target = target_class_id.filter(|_| {
                    checkcast_inline_enabled() && operand_is_trusted_oop && prim_array_tag.is_none()
                });
                if checkcast_inline_enabled() {
                    use std::sync::atomic::Ordering::Relaxed;
                    // Name the refusal per CAUSE. "Not inlined" is a
                    // verdict; these two are the reasons, and they want
                    // opposite fixes.
                    if prim_array_tag.is_some() {
                        crate::CHECKCAST_INLINE_SITES_PRIM_ARRAY.fetch_add(1, Relaxed);
                    } else if inline_target.is_some() {
                        crate::CHECKCAST_INLINE_SITES.fetch_add(1, Relaxed);
                    } else if target_class_id.is_none() {
                        crate::CHECKCAST_INLINE_NO_TARGET_ID.fetch_add(1, Relaxed);
                    } else {
                        crate::CHECKCAST_INLINE_UNTRUSTED.fetch_add(1, Relaxed);
                    }
                    if inline_target.is_none()
                        && prim_array_tag.is_none()
                        && cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_CHECKCAST_INLINE")
                    {
                        eprintln!(
                            "[checkcast-inline] pc={pc} REFUSED target_id={target_class_id:?} have_key={trusted_have_key} marks_exact={trusted_marks_exact} top_is_oop={trusted_top_is_oop} method={}",
                            self.method_key,
                        );
                    }
                }
                let mut hit_patches: Vec<usize> = Vec::new();
                if let Some(tag) = prim_array_tag {
                    // ONE comparison settles a 1-D primitive array: the
                    // KIND_TAGS byte is `kind | (element_type << 2)` with
                    // bits 6..7 reserved zero, so `byte[]` is exactly 0x21
                    // and nothing else is. No null test is needed BEFORE it
                    // — but the load would fault on null, so it still comes
                    // first.
                    self.load_slot_to_reg(RAX, obj_slot);
                    let mut slow = self.emit_trusted_oop_receiver_check();
                    self.buf.emit(&[
                        0x80,
                        0x78,
                        cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                        tag,
                    ]);
                    hit_patches.push(self.emit_jcc_rel32_patch(0x84)); // JE → done
                    for p in slow.drain(..) {
                        self.patch_rel32_to_here(p);
                    }
                } else if let Some(target_class_id) = inline_target {
                    self.load_slot_to_reg(RAX, obj_slot);
                    // Null is a LEGAL cast and the helper already returns 0
                    // for it, so null goes to the helper rather than growing
                    // a second null path here. `emit_trusted_oop_receiver_check`
                    // is that bare `TEST/JZ`, shared with the `getfield` arm.
                    // ARRAY RECEIVERS MUST NOT REACH THE COMPARE.
                    // `regression-suite` `RJitArrayTypecheck` caught this
                    // version of the fix before it left the branch, and the
                    // vector it caught it with is the one written for
                    // BUG-JIT-ARRAY-INSTANCEOF-20260726 — the same defect,
                    // reintroduced by the same reasoning.
                    //
                    // An array's header does NOT hold its own class id: a
                    // reference array holds its COMPONENT's, and a primitive
                    // array holds 0 (it has no class entry at all). So
                    // `checkcast java/lang/String` on a `String[]` receiver
                    // finds `String`'s id in the header, matches the baked
                    // target, and accepts a cast that must throw. That is
                    // exactly why the runtime helper screens its own
                    // recorded-id shortcut with `!recv_is_array` and answers
                    // arrays from `array_descriptor_of` instead.
                    //
                    // One instruction settles it: a plain object's KIND_TAGS
                    // byte is zero — `ObjectKind::Object` and
                    // `ArrayElementType::Reference` are both 0, pinned by
                    // `const _: () = assert!` in `heap_types.rs` precisely so
                    // JIT guards may do this. Arrays take the helper, which
                    // is authoritative for them.
                    let mut slow = self.emit_trusted_oop_receiver_check();
                    // CMP BYTE [RAX+KIND_TAGS_BYTE_OFFSET], 0 (80 /7 ib,
                    // ModRM 0x78 = mod01 disp8 /7 rm=RAX).
                    self.buf.emit(&[
                        0x80,
                        0x78,
                        cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                        0x00,
                    ]);
                    slow.push(self.emit_jcc_rel32_patch(0x85)); // JNE → helper
                                                                // CMP DWORD [RAX+class_id_off], target_class_id.
                                                                // 81 /7 id with ModRM 0xB8 = mod10 (disp32) /7 rm=RAX —
                                                                // the disp32 twin of the `0x81, 0x78` (disp8) form the
                                                                // guarded-virtual inline arm emits.
                    let cid_off = self.helpers.class_id_offset_in_obj as i32; // Cast: x86-64 disp32
                    let cid_off_bytes = cid_off.to_le_bytes();
                    let target_bytes = target_class_id.to_le_bytes();
                    self.buf.emit(&[0x81, 0xB8]);
                    self.buf.emit(&cid_off_bytes);
                    self.buf.emit(&target_bytes);
                    // Equal ⇒ the receiver IS the target class ⇒ the cast
                    // succeeds and its result is the reference we already
                    // hold, which is exactly what the helper would return.
                    hit_patches.push(self.emit_jcc_rel32_patch(0x84)); // JE → done
                    for p in slow.drain(..) {
                        self.patch_rel32_to_here(p);
                    }
                }

                // Call jit_checkcast(vm_ptr, obj_ptr, class_name_ptr, class_name_len) → obj_ptr
                self.emit_load_local(ARG_REGS[0], self.heap_local_offset); // vm_ptr
                self.load_slot_to_reg(ARG_REGS[1], obj_slot); // obj_ptr
                self.emit_mov_imm64(ARG_REGS[2], name_ptr as i64); // class_name_ptr // Cast: JIT ABI convention
                self.emit_mov_imm64(ARG_REGS[3], name_len as i64); // class_name_len // Cast: JIT ABI convention
                                                                   // Round-8 wave-3: defensive callee-saved spill
                                                                   // before any GC-triggering CALL.
                self.emit_pre_safepoint_spill();
                self.emit_call_absolute(self.helpers.checkcast);
                // T1.1.2 — checkcast may resolve the target class
                // on demand (first access) which allocates a
                // `java/lang/Class` mirror. That's a GC-triggering
                // safepoint — emit the oop map before pushing.
                self.emit_oop_map_for_safepoint();
                // Residual-6 companion fix: a definitively-failed cast now
                // stashes a ClassCastException and returns the i64::MIN
                // sentinel (see `jit_checkcast`) instead of a silent null.
                // Route the sentinel through the shared exception stub like
                // every other fallible helper; `emitted_checkcast_throw`
                // forces `has_dispatch` so the helper has the JIT_THREAD
                // TLS it needs to construct the CCE and the entry path
                // drains the pending exception.
                self.emit_post_invoke_exception_check(b'L');
                self.emitted_checkcast_throw = true;
                // Join. The fast path jumps here with the receiver still in
                // RAX — the same value the helper returns on a hit — having
                // skipped the blind spill, the safepoint's oop map and the
                // exception check, none of which a path that makes no call
                // and cannot throw is owed.
                for p in hit_patches {
                    self.patch_rel32_to_here(p);
                }
                // Result (obj_ptr or 0 for null) is in RAX — push onto stack
                self.push_from_rax();
                // checkcast returns the same reference (or null).
                self.mark_top_as_oop();
                pc += 3;
            }

            // instanceof (0xc1) — type check (returns 0 or 1)
            0xc1 => {
                self.flush_scratch_registers();
                // Look up resolved typecheck info for this PC
                // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                let (_, name_ptr, name_len) = self
                    .typecheck_info_idx
                    .get(&pc)
                    .map(|&i| self.typecheck_info[i])
                    .unwrap_or((pc, std::ptr::null(), 0));
                // Same as `checkcast`: with no resolved target the helper
                // answers `false` for every object, a wrong answer rather
                // than a missing one. Stay interpreted.
                if name_ptr.is_null() || name_len == 0 {
                    self.fail("singlepass-codegen/instanceof-unresolved-site");
                    pc += 3;
                    return WalkStep::Next(pc);
                }

                // ---- inline fast path: checkcast's guards, instanceof's answers ----
                //
                // Exactly the preconditions of the `checkcast` arm above,
                // read the same way and for the same reasons (see the long
                // comments there): the target id comes from the site's
                // interned name, the operand must be a trusted oop, and the
                // whole thing sits under `checkcast_inline_enabled`. Only
                // the ANSWERS differ, and instanceof has more of them to
                // give without a call:
                //
                // * null — `instanceof` is 0 for null (JVMS §6.5), so null
                //   needs no helper at all. (checkcast sends null to the
                //   helper only because it has no second null path.)
                // * a plain object whose header class id IS the target — 1.
                //   Arrays are screened out by the `KIND_TAGS == 0` compare
                //   first: an array header carries its component's id (or
                //   0), BUG-JIT-ARRAY-INSTANCEOF-20260726.
                // * a 1-D primitive array whose KIND_TAGS byte is the
                //   target's — 1. Primitive array types have no subtypes.
                // * anything else — the helper, emitted unchanged below.
                //
                // The two call-free answers are stubs placed AFTER the
                // helper call, so the call sequence is emitted in the same
                // linear position (and byte-for-byte the same) as before,
                // and the stubs are compiled against the post-call state,
                // in which RAX is the result on every edge into the join.
                // No counter and no debug flag of its own: the checkcast
                // counters describe checkcast, and this arm adds no state.
                let target_class_id =
                    crate::typecheck_target_for_site(name_ptr).filter(|&id| id != 0);
                // Read BEFORE the pop, as in the checkcast arm.
                let operand_is_trusted_oop = !self.method_key.is_empty()
                    && self.stack_oop_marks_exact
                    && self.stack_oop_marks.last().copied().unwrap_or(false);

                let obj_slot = self.pop_stack();

                // SAFETY: `name_ptr`/`name_len` are an `intern_typecheck_target`
                // pair, leaked for the life of the process.
                let prim_array_tag = unsafe { crate::typecheck_site_name(name_ptr, name_len) }
                    .and_then(cratonvm_types::primitive_array_kind_tags_byte)
                    .filter(|_| checkcast_inline_enabled() && operand_is_trusted_oop);
                let inline_target = target_class_id.filter(|_| {
                    checkcast_inline_enabled() && operand_is_trusted_oop && prim_array_tag.is_none()
                });
                let mut null_patches: Vec<usize> = Vec::new();
                let mut true_patches: Vec<usize> = Vec::new();
                if prim_array_tag.is_some() || inline_target.is_some() {
                    self.load_slot_to_reg(RAX, obj_slot);
                    // TEST RAX, RAX ; JZ → the `0` stub.
                    null_patches = self.emit_trusted_oop_receiver_check();
                    let mut slow: Vec<usize> = Vec::new();
                    if let Some(tag) = prim_array_tag {
                        // CMP BYTE [RAX+KIND_TAGS_BYTE_OFFSET], tag ; JE → `1`.
                        self.buf.emit(&[
                            0x80,
                            0x78,
                            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                            tag,
                        ]);
                        true_patches.push(self.emit_jcc_rel32_patch(0x84)); // JE → `1`
                    } else if let Some(target_class_id) = inline_target {
                        // CMP BYTE [RAX+KIND_TAGS_BYTE_OFFSET], 0 ; JNE → helper.
                        self.buf.emit(&[
                            0x80,
                            0x78,
                            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                            0x00,
                        ]);
                        slow.push(self.emit_jcc_rel32_patch(0x85)); // JNE → helper
                                                                    // CMP DWORD [RAX+class_id_off], target_class_id ;
                                                                    // JE → `1`. Same `81 B8 disp32 imm32` form as checkcast.
                        let cid_off = self.helpers.class_id_offset_in_obj as i32; // Cast: x86-64 disp32
                        self.buf.emit(&[0x81, 0xB8]);
                        self.buf.emit(&cid_off.to_le_bytes());
                        self.buf.emit(&target_class_id.to_le_bytes());
                        true_patches.push(self.emit_jcc_rel32_patch(0x84)); // JE → `1`
                    }
                    // Fall-through and every JNE: the helper.
                    for p in slow {
                        self.patch_rel32_to_here(p);
                    }
                }

                // Call jit_instanceof(vm_ptr, obj_ptr, class_name_ptr, class_name_len) → 0/1
                self.emit_load_local(ARG_REGS[0], self.heap_local_offset); // vm_ptr
                self.load_slot_to_reg(ARG_REGS[1], obj_slot); // obj_ptr
                self.emit_mov_imm64(ARG_REGS[2], name_ptr as i64); // class_name_ptr // Cast: JIT ABI convention
                self.emit_mov_imm64(ARG_REGS[3], name_len as i64); // class_name_len // Cast: JIT ABI convention
                                                                   // Round-8 wave-3: defensive callee-saved spill
                                                                   // before any GC-triggering CALL.
                self.emit_pre_safepoint_spill();
                self.emit_call_absolute(self.helpers.instanceof_check);
                self.emitted_instanceof_call = true;
                // T1.1.2 — instanceof may resolve the target class
                // on demand, allocating a Class mirror. Emit the
                // oop map even though the return value is a
                // primitive int.
                self.emit_oop_map_for_safepoint();
                if !null_patches.is_empty() || !true_patches.is_empty() {
                    // Helper result in RAX: skip the two stubs.
                    let past_helper = self.emit_jmp_rel32_patch();
                    let mut join: Vec<usize> = vec![past_helper];
                    if !null_patches.is_empty() {
                        for p in null_patches {
                            self.patch_rel32_to_here(p);
                        }
                        // XOR EAX, EAX — null is not an instance of anything.
                        self.emit_xor_reg_self(RAX);
                        if !true_patches.is_empty() {
                            join.push(self.emit_jmp_rel32_patch());
                        }
                    }
                    if !true_patches.is_empty() {
                        for p in true_patches {
                            self.patch_rel32_to_here(p);
                        }
                        // MOV RAX, 1 — the receiver is exactly the target.
                        self.emit_mov_imm32_sx(RAX, 1);
                    }
                    for p in join {
                        self.patch_rel32_to_here(p);
                    }
                }
                // Result (0 or 1) is in RAX — push onto stack
                self.push_from_rax();
                pc += 3;
            }

            // multianewarray — allocate multi-dimensional array (2D only)
            0xc5 => {
                self.flush_scratch_registers();
                let _cp_hi = code[pc + 1];
                let _cp_lo = code[pc + 2];
                let ndims = code[pc + 3] as usize; // Widening: always safe
                debug_assert_eq!(ndims, 2);

                // Pop dimensions: top of stack = last dimension
                let dim2_slot = self.pop_stack(); // inner dimension
                let dim1_slot = self.pop_stack(); // outer dimension

                // The packed `(holder_class_id | cp_idx << 32)` site
                // descriptor for this pc. The helper resolves the array
                // class from it at run time, loader-faithfully, through the
                // same `interpreter::multianewarray_alloc` the interpreter
                // uses — so both tiers stamp the same component classes
                // into the allocated levels.
                //
                // This used to be a bare leaf element-type code, which
                // carried no class at all; the helper then allocated every
                // level with `ClassId(0)` and a compiled `new String[a][b]`
                // came back as `[Ljava.lang.Object;`. A site with no entry
                // cannot be compiled correctly at all now (there is no
                // "default" array class), so bail rather than emit a call
                // that would allocate the wrong type.
                let Some(&(_, site)) = self.multianewarray_info.iter().find(|(p, _)| *p == pc)
                else {
                    return WalkStep::Return(false);
                };

                // Call jit_multianewarray_2d(heap_ptr, site, dim1, dim2)
                self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                self.emit_mov_imm64(ARG_REGS[1], site);
                self.load_slot_to_reg(ARG_REGS[2], dim1_slot);
                self.load_slot_to_reg(ARG_REGS[3], dim2_slot);
                // Round-8 wave-3: defensive callee-saved spill
                // before any GC-triggering CALL.
                self.emit_pre_safepoint_spill();
                self.emit_call_absolute(self.helpers.multianewarray_2d);
                // Resolution can run a user `ClassLoader.loadClass`, i.e.
                // arbitrary Java on this thread — republish the frame
                // afterwards exactly as the `new`/`anewarray` CP-indexed
                // arms do.
                crate::runtime_lowering::emit_post_call_frame_republish(
                    &mut self.buf,
                    self.helpers.frame_record,
                );
                // T1.1.2 — multianewarray is a GC-triggering safepoint.
                self.emit_oop_map_for_safepoint();
                // Negative dimension / OOM / failed resolution all come back
                // as the 0/null sentinel with a pending exception; bail into
                // the method's exception table instead of pushing the null
                // and dereferencing it.
                self.emit_post_alloc_oom_check();
                self.push_from_rax();
                // The result is a reference array.
                self.mark_top_as_oop();
                pc += 4;
            }

            // monitorenter / monitorexit: exact lock elision followed by
            // the direct thin-lock runtime stub for every live receiver.
            0xC2 | 0xC3 => {
                if self.sr_monitor_scalar_ops.contains(&pc) {
                    // Proven scalar receiver: lock cannot be observed or
                    // contended. Phase C records its depth for deopt relock.
                    let _ = self.pop_stack();
                    pc += 1;
                    return WalkStep::Next(pc);
                }

                // The old "any scalar replacement in this method" test
                // could elide a lock on an unrelated escaping receiver.
                // Only the exact per-PC proof above may remove the lock.
                let helper = if op == 0xC2 {
                    self.direct_helpers.monitor_enter
                } else {
                    self.direct_helpers.monitor_exit
                };
                if helper == 0 || !self.needs_heap {
                    return WalkStep::Return(false);
                }
                // Keep the receiver on the abstract stack while publishing
                // safepoint roots; moving GC can then rewrite its shadow
                // home during a contended enter. Pop only after the push.
                self.flush_scratch_registers();
                self.emit_pre_safepoint_spill();
                let recv_slot = self.pop_stack();
                let recv_offset = match recv_slot {
                    StackSlot::Frame(offset) => offset,
                    StackSlot::CalleeSaved(reg) | StackSlot::Scratch(reg, ..) => {
                        let Some(offset) = self.reserve_spill_slots(1, SpillReason::HelperArgs)
                        else {
                            return WalkStep::Return(false);
                        };
                        self.emit_store_local(offset, reg);
                        offset
                    }
                    StackSlot::Xmm(_) => return WalkStep::Return(false),
                };
                crate::runtime_lowering::emit_monitor_stub(
                    &mut self.buf,
                    self.heap_local_offset,
                    recv_offset,
                    helper,
                    self.helpers.frame_record,
                );
                self.emitted_monitor_call = true;
                self.emit_oop_map_for_safepoint();
                self.emit_post_invoke_exception_check(b'V');
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
