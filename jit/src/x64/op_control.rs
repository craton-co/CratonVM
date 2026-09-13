// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Branches, switches, returns and `athrow` in the single-pass backend's bytecode walk.
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
    pub(super) fn walk_control(
        &mut self,
        code: &[u8],
        code_len: usize,
        op: u8,
        mut pc: usize,
        dead: &mut bool,
        branch_targets: &[bool],
    ) -> WalkStep {
        match op {

            // ifeq..ifle (0x99..0x9e) — compare int against zero
            0x99..=0x9e => {
                self.flush_scratch_registers();
                let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32; // Widening: always safe
                let target_pc = match pc.checked_add_signed(offset as isize) {
                    // Cast: address arithmetic
                    Some(t) => t,
                    None => return WalkStep::Return( false), // invalid branch target
                };
                if target_pc <= pc {
                    self.emit_safepoint_poll();
                }

                // Canonicalize for forward merge points BEFORE popping the
                // operand, so it participates in the relocation. Popping
                // first left the operand's frame slot invisible to
                // canonicalize_stack(), which could store a remaining
                // register-resident slot to that same offset (register
                // slots shift the offsets of Frame slots above them down)
                // and clobber the operand before the TEST below read it.
                if target_pc > pc && self.stack.len() > 1 {
                    self.canonicalize_stack();
                }
                let slot = self.pop_stack();
                // TEST r32, r32 — sets ZF/SF for comparison against zero
                let reg = self.slot_to_gpr(slot, RCX);
                self.emit_test_r32_r32(reg);

                let cc = match op {
                    0x99 => 0x84, // JE
                    0x9a => 0x85, // JNE
                    0x9b => 0x8C, // JL
                    0x9c => 0x8D, // JGE
                    0x9d => 0x8F, // JG
                    0x9e => 0x8E, // JLE
                    _ => unreachable!(),
                };

                // PGO branch prediction hint prefix (Intel Architecture Manual 2.4.4).
                // 0x3E = DS prefix = "branch taken" hint.
                // 0x2E = CS prefix = "branch not taken" hint.
                if let Some(&is_taken) = self.branch_hints.get(&pc) {
                    self.buf.emit_byte(if is_taken { 0x3E } else { 0x2E });
                }
                self.buf.emit_byte(0x0F);
                self.buf.emit_byte(cc);
                let patch_offset = self.buf.pos();
                self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                self.forward_patches.push((patch_offset, target_pc));
                self.record_branch_target_depth(target_pc);
                self.reset_spills();
                pc += 3;
            }

            // if_icmpeq..if_icmple (0x9f..0xa4)
            0x9f..=0xa4 => {
                self.flush_scratch_registers();
                let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32; // Widening: always safe
                let target_pc = match pc.checked_add_signed(offset as isize) {
                    // Cast: address arithmetic
                    Some(t) => t,
                    None => return WalkStep::Return( false), // invalid branch target
                };
                if target_pc <= pc {
                    self.emit_safepoint_poll();
                }

                // Canonicalize for forward merge points BEFORE popping the
                // operands (see ifeq..ifle above): with both operands still
                // on the simulated stack they are relocated above every
                // store target, so a remaining register-resident slot can
                // no longer be stored over a popped operand's frame slot.
                // Done before the cmov consult too — the peephole then sees
                // frame-canonical operands in this rare deep-stack shape,
                // which costs a reload but stays correct.
                if target_pc > pc && self.stack.len() > 2 {
                    self.canonicalize_stack();
                }
                let val2 = self.pop_stack(); // value2
                let val1 = self.pop_stack(); // value1

                // peephole-cmov (Round-11 HIGH-3): user-written
                // min/max pattern → CMOV. The peephole consumes
                // the if_icmp, the fall-through iload, the goto,
                // and the taken-side iload all at once; on hit
                // we resume at the merge PC L2.
                if let Some(new_pc) = self.try_cmov_minmax_peephole(
                    code,
                    code_len,
                    &branch_targets,
                    pc,
                    op,
                    val1,
                    val2,
                ) {
                    // Map the original if_icmp PC to the start of
                    // the CMOV sequence so downstream branch
                    // resolution keeps working.
                    pc = new_pc;
                    self.reset_spills();
                    return WalkStep::Next(pc);
                }

                // Emit CMP with direct reg-reg when possible
                let r1 = self.slot_to_gpr(val1, RAX);
                let r2 = self.slot_to_gpr(val2, RCX);
                self.emit_cmp_r32_r32(r1, r2);

                let cc = match op {
                    0x9f => 0x84, // JE
                    0xa0 => 0x85, // JNE
                    0xa1 => 0x8C, // JL
                    0xa2 => 0x8D, // JGE
                    0xa3 => 0x8F, // JG
                    0xa4 => 0x8E, // JLE
                    _ => unreachable!(),
                };

                // PGO branch prediction hint (same encoding as ifeq..ifle above).
                if let Some(&is_taken) = self.branch_hints.get(&pc) {
                    self.buf.emit_byte(if is_taken { 0x3E } else { 0x2E });
                }
                self.buf.emit_byte(0x0F);
                self.buf.emit_byte(cc);
                let patch_offset = self.buf.pos();
                self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                self.forward_patches.push((patch_offset, target_pc));
                self.record_branch_target_depth(target_pc);
                self.reset_spills();
                pc += 3;
            }

            // goto
            0xa7 => {
                let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32; // Widening: always safe
                let target_pc = match pc.checked_add_signed(offset as isize) {
                    // Cast: address arithmetic
                    Some(t) => t,
                    None => return WalkStep::Return( false), // invalid branch target
                };
                // Flush scratch registers before any branch -- they are
                // caller-saved and not valid across basic block boundaries.
                self.flush_scratch_registers();
                // For forward branches with non-empty stack, canonicalize the
                // stack layout so all paths to the target use the same offsets.
                if target_pc > pc && !self.stack.is_empty() {
                    self.canonicalize_stack();
                }

                // Loop unrolling: if this is a back-edge of an unrollable loop,
                // copy the native code for the loop body as extra iterations
                let unroll_copies = if target_pc <= pc {
                    self.unroll_loops
                        .iter()
                        .find(|&&(_, be, _)| be == pc)
                        .map(|&(_, _, copies)| copies)
                } else {
                    None
                };

                if let Some(extra_copies) = unroll_copies {
                    // Copy the native code from header to here for extra iterations
                    let header_native = self.pc_to_native[target_pc];
                    if header_native >= 0 {
                        let body_start = header_native as usize; // Cast: address arithmetic
                        let body_end = self.buf.pos();
                        let body_len = body_end - body_start;

                        if body_len > 0 && body_len < 4096 {
                            // Snapshot the original body bytes once
                            let body_bytes: Vec<u8> =
                                self.buf.as_slice()[body_start..body_end].to_vec();

                            // Snapshot every patch vector entry whose
                            // native offset falls inside the original
                            // body span. Each duplicated copy needs a
                            // shifted twin for every snapshot entry so
                            // late-stage stub emission / branch
                            // resolution covers the copies too.
                            //
                            // Task #60: this is the deep follow-up that
                            // removes the previous allow-list — earlier
                            // unrollers shifted only `bounds_check_stubs`
                            // and gated bodies containing getfield /
                            // invokevirtual / new / athrow / checkcast
                            // behind a safety check. With every patch
                            // vector now shifted AND helper rel32s
                            // re-resolved AND IC slots per-clone, the
                            // duplicator handles arbitrary bytecodes.
                            let orig_patches: Vec<(usize, usize)> = self
                                .forward_patches
                                .iter()
                                .filter(|&&(po, _)| po >= body_start && po < body_end)
                                .copied()
                                .collect();
                            let orig_bounds_stubs: Vec<(usize, usize)> = self
                                .bounds_check_stubs
                                .iter()
                                .filter(|&&(po, _)| po >= body_start && po < body_end)
                                .copied()
                                .collect();
                            let orig_excn_stubs: Vec<(usize, usize)> = self
                                .exception_check_stubs
                                .iter()
                                .filter(|&&(po, _)| po >= body_start && po < body_end)
                                .copied()
                                .collect();
                            // JEP 358: each entry is (action, patch_offset,
                            // trap_key); filter by the offset and carry the
                            // action through. The trap key is DROPPED for
                            // the copies below -- see the `extend` that
                            // re-adds them.
                            let orig_nullstore_stubs: Vec<(u8, usize)> = self
                                .null_check_store_stubs
                                .iter()
                                .filter(|&&(_, po, _)| po >= body_start && po < body_end)
                                .map(|&(action, po, _)| (action, po))
                                .collect();
                            let orig_self_calls: Vec<usize> = self
                                .self_call_patches
                                .iter()
                                .filter(|&&po| po >= body_start && po < body_end)
                                .copied()
                                .collect();
                            // Implicit null checks. A `getfield` whose
                            // receiver could not be proved non-null emits
                            // NO test: the dereference is allowed to fault
                            // and `implicit_null::recover` translates the
                            // SIGSEGV into an NPE by looking the faulting
                            // PC up in a table. A duplicated copy of that
                            // dereference is a DIFFERENT faulting PC, and
                            // until this snapshot existed it was in no
                            // table -- so a null receiver in copy 0 threw
                            // NullPointerException and the same receiver
                            // one iteration later killed the process.
                            //
                            // Not a hypothetical: `walk(N o) { for (int i =
                            // 0; i < 5; i++) { a += o.v; o = o.next; } }`
                            // over a three-element list is an
                            // EXCEPTION_ACCESS_VIOLATION reading 0x0F under
                            // the default configuration, and correct with
                            // `CRATONVM_DISABLE_UNROLL=1`. The bodies are
                            // byte-identical; only the table differs.
                            //
                            // This vector post-dates the Task #60 sweep
                            // above, which is how it came to be the one
                            // patch vector the duplicator did not know
                            // about.
                            let orig_implicit_null: Vec<(usize, usize)> = self
                                .implicit_null_sites
                                .iter()
                                .filter(|&&(fault_off, _)| {
                                    fault_off >= body_start && fault_off < body_end
                                })
                                .copied()
                                .collect();
                            let orig_deopt_stubs: Vec<(usize, usize, i64)> = self
                                .deopt_stubs
                                .iter()
                                .filter(|&&(po, _, _)| po >= body_start && po < body_end)
                                .copied()
                                .collect();
                            let orig_jump_table_patches: Vec<(usize, usize, usize)> = self
                                .jump_table_patches
                                .iter()
                                .filter(|&&(eo, tb, _)| {
                                    // Both the entry slot AND the table base
                                    // must live inside the body for the
                                    // RIP-relative arithmetic to remain
                                    // consistent under a uniform shift. In
                                    // practice tableswitch tables are emitted
                                    // immediately after the dispatch code,
                                    // so this is the common case; any entry
                                    // that straddles the boundary is left for
                                    // the late patcher (which still resolves
                                    // the *original* copy correctly).
                                    eo >= body_start
                                        && eo < body_end
                                        && tb >= body_start
                                        && tb < body_end
                                })
                                .copied()
                                .collect();
                            let orig_oop_maps: Vec<crate::OopMapEntry> = self
                                .oop_maps
                                .iter()
                                .filter(|e| {
                                    let off = e.native_pc_offset as usize; // Widening: u32 → usize
                                    off >= body_start && off < body_end
                                })
                                .cloned()
                                .collect();
                            let orig_helper_calls: Vec<usize> = self
                                .helper_call_patches
                                .iter()
                                .filter(|&&po| po >= body_start && po < body_end)
                                .copied()
                                .collect();
                            // RIP-relative displacements addressing a fixed
                            // absolute target: the safepoint flag, and
                            // since 2026-09-10 the layout-replacement
                            // epoch a getfield site guards on. Same hazard
                            // as the helper rel32 above and the same fix:
                            // verbatim bytes would address
                            // `target + shift` from the copy. The two
                            // carry DIFFERENT trailing-byte counts (the
                            // poll's `imm8`, the guard's `imm32`), which
                            // is why the trail is per entry and not a
                            // constant here.
                            let orig_rip_abs: Vec<(usize, usize)> = self
                                .rip_abs_disp32_patches
                                .iter()
                                .filter(|&&(po, _)| po >= body_start && po < body_end)
                                .copied()
                                .collect();
                            let orig_ic_patches: Vec<(usize, u8, usize)> = self
                                .ic_patches
                                .iter()
                                .filter(|&&(po, _, _)| po >= body_start && po < body_end)
                                .copied()
                                .collect();

                            // Snapshot the buffer's base pointer ONCE here.
                            // `JitBuf::reserve` does not relocate after
                            // `as_ptr()` is observed (see the safety note
                            // on `emit_call_absolute`), so this base is the
                            // same address every copy will resolve against.
                            // Cast: non-negative index/count to usize
                            let buf_base = self.buf.as_ptr() as usize;

                            for _ in 0..extra_copies {
                                let copy_start = self.buf.pos();
                                let shift = copy_start as i32 - body_start as i32; // Cast: x86-64 immediate encoding
                                let shift_us = shift as usize; // Cast: address arithmetic

                                // Copy the raw bytes verbatim.
                                self.buf.emit(&body_bytes);

                                // Handle forward patches: internal ones (target
                                // within the loop body) are resolved immediately
                                // using shifted addresses; external ones are
                                // deferred normally.
                                for &(po, tp) in &orig_patches {
                                    let shifted_po = po + shift_us;
                                    if tp >= target_pc && tp <= pc {
                                        // Internal: resolve now using shifted target
                                        let orig_target = self.pc_to_native[tp];
                                        if orig_target >= 0 {
                                            let shifted_target = orig_target + shift;
                                            let rel = shifted_target - (shifted_po as i32 + 4); // Cast: x86-64 immediate encoding
                                            self.buf.try_patch_i32(shifted_po, rel).ok();
                                            // on Err try_patch_i32 set buf.overflowed; compile bails
                                        }
                                    } else {
                                        // External: defer to normal resolution
                                        self.forward_patches.push((shifted_po, tp));
                                    }
                                }

                                // Re-resolve every helper rel32 in this
                                // copy. The duplicated bytes carry the
                                // *original* rel32 — which, after the
                                // shift, would land at `helper + shift`
                                // (the N-Body Body.x SIGSEGV pattern from
                                // CHANGELOG). Reconstruct the helper
                                // address from the original site and
                                // re-encode the rel32 against the copy's
                                // call PC.
                                for &po in &orig_helper_calls {
                                    // `po` is the offset of the 4-byte
                                    // rel32 within the buffer; the byte
                                    // after the rel32 is `po + 4`, which
                                    // is the reference point for both
                                    // the original and copied rel32
                                    // displacements. The next-PC's
                                    // *runtime* absolute address is
                                    // `buf_base + po + 4`.
                                    // Read the 4-byte rel32 immediate.
                                    // `from_le_bytes` wants an owned
                                    // `[u8; 4]`; copy out of the buffer
                                    // slice explicitly so the immutable
                                    // borrow ends before the upcoming
                                    // `patch_i32` mutable call.
                                    let mut rel_bytes = [0u8; 4];
                                    rel_bytes.copy_from_slice(&self.buf.as_slice()[po..po + 4]);
                                    let orig_rel32 = i32::from_le_bytes(rel_bytes);
                                    let orig_next_pc =
                                        buf_base.wrapping_add(po).wrapping_add(4);
                                    let helper_addr =
                                        // Widening: usize address & i32 rel32 -> i64 (no truncation; rel math)
                                        (orig_next_pc as i64).wrapping_add(orig_rel32 as i64);
                                    let copy_po = po + shift_us;
                                    let copy_next_pc =
                                        buf_base.wrapping_add(copy_po).wrapping_add(4);
                                    let delta: i128 =
                                        // Widening: i64/usize -> i128 (no truncation, for range check)
                                        (helper_addr as i128) - (copy_next_pc as i128);
                                    // Helpers reachable in ±2GB at the
                                    // original site stay reachable at the
                                    // shifted copy (the shift is at most
                                    // body_len < 4096 bytes). Truncating
                                    // to i32 is safe in practice; debug-
                                    // assert to catch any pathological
                                    // future code-cache layout.
                                    debug_assert!(
                                        // Widening: i64/usize -> i128 (no truncation, for range check)
                                        delta >= i32::MIN as i128 && delta <= i32::MAX as i128,
                                        "unrolled helper rel32 out of range",
                                    );
                                    self.buf
                                        .try_patch_i32(copy_po, delta as i32) // Cast: rel32 displacement
                                        .ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                                }

                                // RIP-relative absolute-target
                                // displacements. Identical reasoning to
                                // the helper rel32 above, with one
                                // difference that is easy to get wrong:
                                // the reference point is the end of the
                                // whole instruction, so `trail` (the bytes
                                // emitted AFTER the displacement — an
                                // `imm8` for the safepoint poll's `TEST`)
                                // is part of it.
                                for &(po, trail) in &orig_rip_abs {
                                    let mut d_bytes = [0u8; 4];
                                    d_bytes.copy_from_slice(&self.buf.as_slice()[po..po + 4]);
                                    let orig_disp32 = i32::from_le_bytes(d_bytes);
                                    let orig_next_pc = buf_base
                                        .wrapping_add(po)
                                        .wrapping_add(4)
                                        .wrapping_add(trail);
                                    let target =
                                        // Widening: usize address & i32 disp32 -> i64 (no truncation; rel math)
                                        (orig_next_pc as i64).wrapping_add(orig_disp32 as i64);
                                    let copy_po = po + shift_us;
                                    let copy_next_pc = buf_base
                                        .wrapping_add(copy_po)
                                        .wrapping_add(4)
                                        .wrapping_add(trail);
                                    let delta: i128 =
                                        // Widening: i64/usize -> i128 (no truncation, for range check)
                                        (target as i128) - (copy_next_pc as i128);
                                    // Reachable at the original site stays
                                    // reachable at the copy: the shift is
                                    // at most one loop body.
                                    debug_assert!(
                                        // Widening: i64/usize -> i128 (no truncation, for range check)
                                        delta >= i32::MIN as i128 && delta <= i32::MAX as i128,
                                        "unrolled RIP-relative disp32 out of range",
                                    );
                                    self.buf
                                        .try_patch_i32(copy_po, delta as i32) // Cast: rel32 displacement
                                        .ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                                }

                                // Per-clone MIC/PIC slots. For each IC
                                // site in the original body, mint a fresh
                                // Box<JitMICSlot> / Box<JitPICSlot>, stash
                                // it on the compiler so it outlives the
                                // compiled method, and rewrite the imm64
                                // baked into the duplicated `MOV R10,
                                // imm64` to point at the new slot.
                                //
                                // Without this, every copy shares the
                                // original slot — per-iteration cache
                                // hits collide and miss across copies for
                                // any receiver-type-varying loop.
                                let mut cloned_ic_slots: HashMap<(u8, usize), i64> =
                                    HashMap::new();
                                for &(po, kind, original_ptr) in &orig_ic_patches {
                                    let copy_po = po + shift_us;
                                    let fresh_ptr = *cloned_ic_slots
                                        .entry((kind, original_ptr))
                                        .or_insert_with(|| match kind {
                                            0 => {
                                                let mic = Box::new(crate::JitMICSlot::new());
                                                let p: *const crate::JitMICSlot = &*mic;
                                                self.cloned_mic_slots.push(mic);
                                                p as i64
                                            }
                                            1 => {
                                                let pic = Box::new(crate::JitPICSlot::new());
                                                let p: *const crate::JitPICSlot = &*pic;
                                                self.cloned_pic_slots.push(pic);
                                                p as i64
                                            }
                                            _ => {
                                                unreachable!("unknown ic_patches kind {}", kind)
                                            }
                                        });
                                    // Overwrite the 8-byte little-endian
                                    // imm64 baked into the duplicated
                                    // `MOV R10, imm64` (10-byte form).
                                    let bytes = fresh_ptr.to_le_bytes();
                                    for i in 0..8 {
                                        self.buf.try_patch_byte(copy_po + i, bytes[i]).ok();
                                        // on Err try_patch_byte set buf.overflowed; compile bails
                                    }
                                }

                                // Shift bounds-check, exception-check,
                                // null-check-store, and self-call patch
                                // sites so the late stub emitters see
                                // every duplicated branch.
                                self.bounds_check_stubs.extend(
                                    orig_bounds_stubs
                                        .iter()
                                        .map(|&(po, bci)| (po + shift_us, bci)),
                                );
                                self.exception_check_stubs.extend(
                                    orig_excn_stubs
                                        .iter()
                                        .map(|&(po, bci)| (po + shift_us, bci)),
                                );
                                // Trap key `0`: a copied site keeps the
                                // action (which depends only on the opcode)
                                // and gives up its LINE. The recorded site
                                // describes the body this copy was made
                                // from, and a duplicated body is not
                                // guaranteed to be the same splice -- a
                                // guarded site emits one copy per receiver
                                // variant, each a different callee. Carrying
                                // the key would name one variant's chain on
                                // every copy: a frame naming a method that
                                // did not run, which is the one outcome this
                                // area refuses. A missing line is the other.
                                self.null_check_store_stubs.extend(
                                    orig_nullstore_stubs
                                        .iter()
                                        .map(|&(action, po)| (action, po + shift_us, 0u32)),
                                );
                                self.self_call_patches
                                    .extend(orig_self_calls.iter().map(|&po| po + shift_us));
                                // The RECOVERY address shifts only when it
                                // is itself inside the duplicated span.
                                // Every site this emitter makes recovers at
                                // its own arm's guarded slow path, which is
                                // a few bytes further into the same body --
                                // but an out-of-line recovery would NOT be
                                // duplicated, and sending a copy's fault to
                                // `recovery + shift` would resume it in
                                // whatever happened to be there. That is
                                // the one failure this table can produce
                                // that nothing downstream catches, so the
                                // condition is written rather than assumed.
                                self.implicit_null_sites
                                    .extend(orig_implicit_null.iter().map(
                                        |&(fault, recover)| {
                                            let recover = if recover >= body_start
                                                && recover < body_end
                                            {
                                                recover + shift_us
                                            } else {
                                                recover
                                            };
                                            (fault + shift_us, recover)
                                        },
                                    ));
                                // Deopt stub patches: (patch_offset, bci,
                                // reason). bci and reason are the same
                                // across copies (it's the same logical
                                // safepoint, identified by JVM bci); only
                                // the patch offset shifts. Sharing the
                                // (bci, reason) key lets emit_deopt_stubs
                                // coalesce the duplicated guards onto a
                                // single shared stub.
                                self.deopt_stubs.extend(
                                    orig_deopt_stubs
                                        .iter()
                                        .map(|&(po, bci, reason)| (po + shift_us, bci, reason)),
                                );
                                // Jump-table patches use RIP-relative
                                // offsets stored as i32 from
                                // table_base_native_offset to the target.
                                // When the entry slot AND the table base
                                // are both inside the body span, the
                                // offset is shift-invariant — both move
                                // by the same amount, so the i32 already
                                // emitted in the duplicated bytes is
                                // still correct. Just shift the
                                // (entry_offset, table_base, target_pc)
                                // tuple itself so the late patcher
                                // re-resolves the copy.
                                self.jump_table_patches.extend(
                                    orig_jump_table_patches.iter().map(|&(eo, tb, tpc)| {
                                        (eo + shift_us, tb + shift_us, tpc)
                                    }),
                                );
                                // Oop maps: each entry stashes the
                                // native_pc_offset of the instruction
                                // AFTER a safepoint. The GC root walker
                                // looks up the map by PC, so duplicated
                                // safepoints need their own shifted
                                // entries — same frame slots, new PC.
                                self.oop_maps.extend(orig_oop_maps.iter().map(|e| {
                                    let mut copy = e.clone();
                                    // native_pc_offset is u32; shift
                                    // is i32 but always positive
                                    // (copy_start > body_start), so
                                    // saturate-add via usize for safe
                                    // arithmetic.
                                    // Cast: non-negative index/count to usize
                                    copy.native_pc_offset = (e.native_pc_offset as usize)
                                        .wrapping_add(shift_us)
                                        as u32; // Cast: native_pc_offset width
                                    copy
                                }));
                                // Helper-call patches: track the
                                // duplicated rel32 site so any future
                                // pass that walks helper_call_patches
                                // (e.g. a nested unroll) sees the copy.
                                self.helper_call_patches
                                    .extend(orig_helper_calls.iter().map(|&po| po + shift_us));
                                // Same, for the RIP-relative sites just
                                // re-resolved above.
                                self.rip_abs_disp32_patches.extend(
                                    orig_rip_abs
                                        .iter()
                                        .map(|&(po, trail)| (po + shift_us, trail)),
                                );
                                // IC patches: same idea — record the
                                // shifted imm64 location with its kind
                                // so any later pass can find it.
                                let mut cloned_patches =
                                    Vec::with_capacity(orig_ic_patches.len());
                                for &(po, kind, original_ptr) in &orig_ic_patches {
                                    let Some(&cloned_ptr) =
                                        cloned_ic_slots.get(&(kind, original_ptr))
                                    else {
                                        // An incomplete IC clone would leave generated
                                        // code pointing at the wrong call-site state.
                                        // Reject this compilation and fall back to the
                                        // interpreter instead of publishing unsafe code.
                                        return WalkStep::Return( false);
                                    };
                                    cloned_patches.push((
                                        po + shift_us,
                                        kind,
                                        cloned_ptr as usize,
                                    ));
                                }
                                self.ic_patches.extend(cloned_patches);
                            }
                        }
                    }
                }

                // Cooperative JIT safepoint poll (CRATONVM_JIT_SAFEPOINT_POLLS)
                // -- loop back-edge. `target_pc <= pc` is this codebase's own
                // definition of a `goto`-shaped back edge (mirrors the check
                // just above that drives `unroll_copies`, and
                // `detect_natural_loops`, which finds loop headers the same
                // way). No-op unless the env flag is set AND this is a
                // context method AND the helper table wired the flag
                // address (see `emit_safepoint_poll`'s doc for the current
                // coverage gap: a loop whose only backward branch is a
                // conditional `ifXX`/`if_icmpXX`/`if_acmpXX` is not polled
                // by this first cut).
                if target_pc <= pc {
                    self.emit_safepoint_poll();
                }

                // JMP rel32 to header
                self.buf.emit_byte(0xE9);
                let patch_offset = self.buf.pos();
                self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                self.forward_patches.push((patch_offset, target_pc));
                self.record_branch_target_depth(target_pc);
                self.reset_spills();
                *dead = true;
                pc += 3;
            }

            // tableswitch — jump table for dense tables, CMP chain for small
            0xaa => {
                self.flush_scratch_registers();
                let base_pc = pc;
                // The shared strict decoder (`bytecode_analysis::switch_table`)
                // refuses a table that leaves the method, an entry count past
                // its cap, or a target outside the code. Refusing leaves the
                // method interpreted, as the private decode this replaced did
                // for the out-of-bounds cases it checked.
                let Some(table) = bytecode_analysis::switch_table(code, code_len, base_pc) else {
                    return WalkStep::Return(false);
                };
                let Some(&(low, _)) = table.cases.first() else {
                    return WalkStep::Return(false);
                };
                let count = table.cases.len();
                let targets: Vec<usize> = table.cases.iter().map(|&(_, target)| target).collect();
                let def_target = table.default;
                pc = base_pc + table.len;
                let any_backward =
                    def_target <= base_pc || targets.iter().any(|&target| target <= base_pc);
                // Canonicalize the operands that OUTLIVE this switch, exactly
                // as the `ifeq`/`if_icmp`/`goto` arms do — and before popping
                // the key, so the key participates in the relocation and
                // cannot be clobbered by another slot's move (the same
                // ordering rule those arms state).
                //
                // Every arm of a switch is a branch target, and a target
                // revived from dead code rebuilds the operand stack at the
                // canonical `base_spill_offset + i*8` — a layout NOTHING was
                // establishing here, so any operand this basic block left at
                // a non-canonical offset was read from the wrong slot by
                // every arm. That is the second half of ECJ's
                // `OperandStack.pop(OperandCategory)` miscompile: the inlined
                // `TypeIds.getCategory` result sat above its semantic depth
                // (fixed in `x64/inlining.rs`) and this `tableswitch` was the
                // one branch shape in the walk that did not repair it, so the
                // `if_icmpeq` at the merge compared the raw `TypeBinding.id`
                // (tomcat/ecj-operandstack-*.md).
                //
                // Forward-only, mirroring those arms: a backward target's
                // layout was fixed when the walk emitted it, and relocating
                // to suit a forward merge would disagree with it.
                if !any_backward && self.stack.len() > 1 {
                    self.canonicalize_stack();
                }
                let key_slot = self.pop_stack();
                if any_backward {
                    self.emit_safepoint_poll();
                }
                self.load_slot_to_reg(RAX, key_slot);
                // Every arm of a switch is a branch target, and needs the
                // operand stack live at it recorded exactly the way the
                // `if`/`goto` arms record theirs — the dead-code merge
                // reconstruction reads both the depth and the oop marks
                // from this map. Nothing recorded them before: an arm
                // revived from dead code fell back to depth 0 with
                // all-`false` marks, which is a guess in the depth and the
                // very unsoundness `branch_target_stack_oop_marks`
                // documents in the marks. The key is already popped here,
                // so `self.stack` is exactly what every arm sees.
                self.record_branch_target_depth(def_target);
                for &target in &targets {
                    self.record_branch_target_depth(target);
                }

                if count <= 4 {
                    // Small table: CMP chain (compact code, few comparisons)
                    // Normalize key: SUB EAX, low
                    if low != 0 {
                        self.buf.emit(&[0x2D]); // SUB EAX, imm32
                        self.buf.emit(&low.to_le_bytes());
                    }
                    for (i, &target) in targets.iter().enumerate() {
                        self.buf.emit(&[0x3D]); // CMP EAX, imm32
                        self.buf.emit(&(i as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
                        self.buf.emit(&[0x0F, 0x84]); // JE rel32
                        let patch = self.buf.pos();
                        self.buf.emit(&[0; 4]);
                        self.forward_patches.push((patch, target));
                    }
                    // Default: JMP
                    self.buf.emit_byte(0xE9);
                    let dp = self.buf.pos();
                    self.buf.emit(&[0; 4]);
                    self.forward_patches.push((dp, def_target));
                } else {
                    // Large table: O(1) jump table dispatch
                    //   SUB EAX, low        ; normalize index
                    //   CMP EAX, count      ; bounds check
                    //   JAE default          ; out of range → default
                    //   MOVSXD RCX, [RDX + RAX*4]  ; load relative offset from table
                    //   ADD RCX, RDX        ; compute absolute address
                    //   JMP RCX             ; indirect jump
                    //   <jump table: count * 4 bytes of i32 offsets>

                    // Normalize key: SUB EAX, low
                    if low != 0 {
                        self.buf.emit(&[0x2D]); // SUB EAX, imm32
                        self.buf.emit(&low.to_le_bytes());
                    }
                    // Bounds check: CMP EAX, count; JAE default
                    self.buf.emit(&[0x3D]); // CMP EAX, imm32
                    self.buf.emit(&(count as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
                    self.buf.emit(&[0x0F, 0x83]); // JAE rel32
                    let bounds_patch = self.buf.pos();
                    self.buf.emit(&[0; 4]);
                    self.forward_patches.push((bounds_patch, def_target));

                    // LEA RDX, [RIP + 0]  → points to jump table
                    // We'll emit: LEA RDX, [RIP + disp32] where disp32 will be
                    // patched to point to the table start.
                    self.buf.emit(&[0x48, 0x8D, 0x15]); // LEA RDX, [RIP + disp32]
                    let lea_patch = self.buf.pos();
                    self.buf.emit(&[0; 4]); // placeholder disp32

                    // MOVSXD RCX, [RDX + RAX*4]  ; load table[index]
                    // Encoding: REX.W 0x63 /r with SIB [RDX + RAX*4]
                    self.buf.emit(&[0x48, 0x63, 0x0C, 0x82]); // MOVSXD RCX, [RDX + RAX*4]

                    // ADD RCX, RDX  ; absolute = table_base + offset
                    self.buf.emit(&[0x48, 0x01, 0xD1]); // ADD RCX, RDX

                    // JMP RCX  ; indirect jump
                    self.buf.emit(&[0xFF, 0xE1]); // JMP RCX

                    // Patch LEA: disp32 = table_start - (lea_patch + 4)
                    let table_start = self.buf.pos();
                    let lea_rel = table_start as i32 - (lea_patch as i32 + 4); // Cast: x86-64 rel32 displacement
                    self.buf.try_patch_i32(lea_patch, lea_rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails

                    // Emit jump table: count entries, each i32 offset from table_start
                    for &target in &targets {
                        let entry_offset = self.buf.pos();
                        self.buf.emit(&[0; 4]); // placeholder
                        self.jump_table_patches
                            .push((entry_offset, table_start, target));
                    }
                }
                self.reset_spills();
                *dead = true;
            }

            // lookupswitch — CMP chain for small, binary search for large
            0xab => {
                self.flush_scratch_registers();
                let base_pc = pc;
                // Decoded by the shared strict decoder; see the `tableswitch` arm.
                let Some(table) = bytecode_analysis::switch_table(code, code_len, base_pc) else {
                    return WalkStep::Return(false);
                };
                let npairs = table.cases.len();
                let def_target = table.default;
                pc = base_pc + table.len;
                let pairs = table.cases;
                let any_backward =
                    def_target <= base_pc || pairs.iter().any(|&(_, target)| target <= base_pc);
                // Same canonicalization the `tableswitch` arm above performs,
                // and for the same reason — see the note there. The two
                // switch arms are the only branch shapes in this walk that
                // were not establishing the canonical layout their own
                // targets are revived with.
                if !any_backward && self.stack.len() > 1 {
                    self.canonicalize_stack();
                }
                let key_slot = self.pop_stack();
                if any_backward {
                    self.emit_safepoint_poll();
                }
                self.load_slot_to_reg(RAX, key_slot);
                // Every arm of a switch is a branch target, and needs the
                // operand stack live at it recorded exactly the way the
                // `if`/`goto` arms record theirs — the dead-code merge
                // reconstruction reads both the depth and the oop marks
                // from this map. Nothing recorded them before: an arm
                // revived from dead code fell back to depth 0 with
                // all-`false` marks, which is a guess in the depth and the
                // very unsoundness `branch_target_stack_oop_marks`
                // documents in the marks. The key is already popped here,
                // so `self.stack` is exactly what every arm sees.
                self.record_branch_target_depth(def_target);
                for &(_, target) in &pairs {
                    self.record_branch_target_depth(target);
                }

                // The binary search needs strictly ascending keys. JVMS
                // requires them, but nothing on this path verifies it, and
                // an unsorted table would silently send keys to default.
                let sorted = pairs.windows(2).all(|w| w[0].0 < w[1].0);
                if npairs <= 6 || !sorted {
                    // Small or unsorted: linear CMP chain
                    for &(key, target) in &pairs {
                        self.buf.emit(&[0x3D]); // CMP EAX, imm32
                        self.buf.emit(&key.to_le_bytes());
                        self.buf.emit(&[0x0F, 0x84]); // JE rel32
                        let patch = self.buf.pos();
                        self.buf.emit(&[0; 4]);
                        self.forward_patches.push((patch, target));
                    }
                    // Default: JMP
                    self.buf.emit_byte(0xE9);
                    let dp = self.buf.pos();
                    self.buf.emit(&[0; 4]);
                    self.forward_patches.push((dp, def_target));
                } else {
                    // Large: binary search tree emitted as nested CMP/JL/JG/JE
                    // The keys in lookupswitch are sorted per JVM spec.
                    // We emit a balanced binary search: O(log n) comparisons.
                    //
                    // Value in EAX. We use a recursive emission strategy:
                    //   pick middle key, CMP EAX, mid_key
                    //   JE target
                    //   JL left_subtree
                    //   (fall through to right subtree)
                    // At leaves, fall through to default.
                    self.emit_binary_search_lookup(&pairs, def_target);
                }
                self.reset_spills();
                *dead = true;
            }

            // ireturn / lreturn / freturn / dreturn / areturn
            0xac..=0xb0 => {
                self.flush_scratch_registers();
                self.pop_to_rax();
                if code[pc] == 0xac {
                    // JVMS §6.5: a `boolean`/`byte`/`char`/`short` return is
                    // narrowed at `ireturn`. A compiled caller reads RAX
                    // raw, so without this a `()Z` body returning 2 hands
                    // it 2. The return type is the method key's; an empty
                    // key (the legacy test wrapper) narrows nothing.
                    let tag = crate::narrowed_int_return_tag(&self.method_key);
                    self.emit_narrow_int_return(tag);
                }
                self.emit_epilogue();
                self.reset_spills();
                *dead = true;
                pc += 1;
            }

            // return (void)
            0xb1 => {
                // No return value needed, just emit epilogue.
                self.flush_scratch_registers();
                // Zero RAX on the normal void-return path so callers can
                // reliably distinguish a clean return (RAX == 0) from the
                // `i64::MIN` deopt sentinel. Without this RAX is whatever
                // the last op left, which could spuriously equal i64::MIN
                // and trip the caller's post-invoke exception guard / the
                // interpreter's post-JIT deopt check.
                // XOR EAX, EAX  (31 C0) — zero-extends to RAX.
                self.buf.emit(&[0x31, 0xC0]);
                self.emit_epilogue();
                self.reset_spills();
                *dead = true;
                pc += 1;
            }

            // athrow (RBC.6) — lower to "stash the exception object as
            // the pending JIT exception, then return the i64::MIN deopt
            // sentinel". The helper (`jit_throw_exception`) handles the
            // JVMS athrow-on-null case by setting the pending-NPE flag
            // instead. The interpreter's JIT-return drains route the
            // exception to the caller; compilation is gated upstream to
            // methods with NO local exception handlers (this lowering
            // cannot branch to an in-method handler) and the OSR
            // trigger declines athrow methods entirely (its bail path
            // resumes at the back-edge and could re-run side effects).
            // Mirrors the shared bounds-check stub, which calls
            // `helpers.throw_aioobe` and epilogues with the sentinel.
            0xbf => {
                self.flush_scratch_registers();
                // Exception ref → first argument register.
                let exc_slot = self.pop_stack();
                self.load_slot_to_reg(ARG_REGS[0], exc_slot);
                // RBC.6 correctness fix — pass this athrow's own bytecode
                // pc as the second argument (a compile-time immediate) so
                // `jit_throw_exception` can stash it alongside the
                // exception. `execute_jit_call` then gives
                // `route_jit_exception_through_method` a real `throw_pc`
                // instead of `usize::MAX`, which — with 2+ exception-table
                // entries whose catch types are in a subtype relationship —
                // can match the wrong entry regardless of which
                // try-region actually threw. `pc` here is this
                // instruction's own bci (loaded after ARG_REGS[0] so it
                // does not disturb the exception-ref load above).
                //
                // `route_jit_exception_through_method` range-tests this
                // against the INTERPRETER exception table, so it must be an
                // interpreter bci — hence `orig_bci`, which is the identity
                // unless this compile is emitting rewritten bytecode. This
                // is the fourth and last site in the backend that bakes a
                // bci as an immediate; see `Compiler::bci_provenance`.
                let throw_bci = self.orig_bci(pc);
                self.emit_mov_imm32_sx(ARG_REGS[1], throw_bci as i32); // Cast: bci fits i32
                self.emit_call_absolute(self.helpers.throw_exception);
                // Helper returned the i64::MIN sentinel in RAX -
                // propagate it as the method's return value.
                //
                // RBC.6 `athrow` admission: inside a protected range the
                // sentinel alone is not enough. `jit_throw_exception` has
                // stashed the exception and this bci, but nothing has
                // recorded where this frame's non-parameter locals live, so
                // a handler that reads one would resume it as 0/null. Route
                // through the reason-9 stub instead of returning directly:
                // it spills the trapping registers, materializes the precise
                // exceptional frame from the snapshot recorded here, and
                // then runs exactly the epilogue this arm would have run.
                // The unconditional `JMP rel32` is patched by
                // `emit_deopt_stubs` the same way a `Jcc rel32` guard is -
                // both end in the same four displacement bytes.
                //
                // `flush_scratch_registers` above ran before the call, so
                // any local the snapshot places in a caller-saved register
                // has already been spilled to its frame slot; this is the
                // same ordering `emit_post_invoke_exception_check` relies on.
                //
                // Keyed on the EMITTER pc, never on `throw_bci`: every
                // `*_box_ptr_by_bci` map, `build_and_record_deopt_point`'s
                // analysis lookups and `emit_deopt_stubs`' stub sharing are
                // all in emitter coordinates, and each applies `orig_bci`
                // itself for the value it hands the runtime. Handing an
                // already-translated bci in would double-apply it under a
                // bytecode loop rewrite (identity, and byte-identical, on an
                // ordinary compile).
                let precise_athrow_stub =
                    self.precise_exception_frames && self.pc_is_protected(pc);
                if precise_athrow_stub {
                    if !self.exc_frame_box_ptr_by_bci.contains_key(&pc) {
                        let box_ptr = self.build_and_record_deopt_point(
                            pc,
                            crate::deopt::DeoptReason::PendingException,
                        );
                        self.exc_frame_box_ptr_by_bci.insert(pc, box_ptr);
                    }
                    // JMP rel32 (E9) - patched to the reason-9 stub, or to
                    // this bci's local-handler stub when this method's own
                    // exception table can catch here and compiled local
                    // handlers are armed. A `throw` caught by the very
                    // method that raised it is the shape javac emits for
                    // every `try { ... throw ... } catch` and for a
                    // rethrowing `finally`, and it is as enterable in
                    // compiled code as a callee's throw: the helper takes
                    // the throwable `jit_throw_exception` just stashed.
                    // The stub's own miss edge is this same reason-9 stub,
                    // so a propagating throw is unchanged.
                    self.buf.emit_byte(0xE9);
                    let patch_offset = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    self.record_exception_check_edge(patch_offset, pc, true);
                } else {
                    self.emit_epilogue();
                }
                self.reset_spills();
                self.emitted_athrow = true;
                *dead = true;
                pc += 1;
            }

            // if_acmpeq (0xa5) — reference equality branch
            0xa5 => {
                self.flush_scratch_registers();
                let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32; // Widening: always safe
                let target_pc = match pc.checked_add_signed(offset as isize) {
                    // Cast: address arithmetic
                    Some(t) => t,
                    None => return WalkStep::Return( false), // invalid branch target
                };
                if target_pc <= pc {
                    self.emit_safepoint_poll();
                }

                // Canonicalize for forward merge points BEFORE popping the
                // operands (see ifeq..ifle). if_acmp historically skipped
                // both the canonicalization and the depth record, so a
                // taken edge with a non-empty remaining stack reached a
                // merge whose layout the two paths never agreed on
                // (surfacing as a simulated-stack underflow that bailed
                // the whole method to the interpreter).
                if target_pc > pc && self.stack.len() > 2 {
                    self.canonicalize_stack();
                }
                let val2 = self.pop_stack();
                let val1 = self.pop_stack();
                self.load_slot_to_reg(RCX, val2);
                self.load_slot_to_reg(RAX, val1);
                // CMP RAX, RCX (REX.W + 0x39 /r)
                self.rex_w();
                self.buf.emit(&[0x39, 0xC8]); // CMP RAX, RCX

                // JE rel32
                self.buf.emit_byte(0x0F);
                self.buf.emit_byte(0x84); // JE
                let patch_offset = self.buf.pos();
                self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                self.forward_patches.push((patch_offset, target_pc));
                self.record_branch_target_depth(target_pc);
                self.reset_spills();
                pc += 3;
            }

            // if_acmpne (0xa6) — reference inequality branch
            0xa6 => {
                self.flush_scratch_registers();
                let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32; // Widening: always safe
                let target_pc = match pc.checked_add_signed(offset as isize) {
                    // Cast: address arithmetic
                    Some(t) => t,
                    None => return WalkStep::Return( false), // invalid branch target
                };
                if target_pc <= pc {
                    self.emit_safepoint_poll();
                }

                // Canonicalize for forward merge points BEFORE popping the
                // operands (see if_acmpeq above).
                if target_pc > pc && self.stack.len() > 2 {
                    self.canonicalize_stack();
                }
                let val2 = self.pop_stack();
                let val1 = self.pop_stack();
                self.load_slot_to_reg(RCX, val2);
                self.load_slot_to_reg(RAX, val1);
                // CMP RAX, RCX (REX.W + 0x39 /r)
                self.rex_w();
                self.buf.emit(&[0x39, 0xC8]); // CMP RAX, RCX

                // JNE rel32
                self.buf.emit_byte(0x0F);
                self.buf.emit_byte(0x85); // JNE
                let patch_offset = self.buf.pos();
                self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                self.forward_patches.push((patch_offset, target_pc));
                self.record_branch_target_depth(target_pc);
                self.reset_spills();
                pc += 3;
            }

            // ifnull (0xc6) — branch if reference is null
            0xc6 => {
                self.flush_scratch_registers();
                let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32; // Widening: always safe
                let target_pc = match pc.checked_add_signed(offset as isize) {
                    // Cast: address arithmetic
                    Some(t) => t,
                    None => return WalkStep::Return( false), // invalid branch target
                };
                if target_pc <= pc {
                    self.emit_safepoint_poll();
                }

                // Canonicalize for forward merge points BEFORE popping the
                // operand (see ifeq..ifle): popping first could let the
                // relocation of a remaining register-resident slot clobber
                // the operand's frame slot before the TEST reads it.
                if target_pc > pc && self.stack.len() > 1 {
                    self.canonicalize_stack();
                }
                let slot = self.pop_stack();
                // HIGH-1 / Fix 1 — wire null-check elimination.
                // If the value on top of stack came from an aload of a
                // local that is proven non-null at this PC, the TEST
                // can never be zero so `ifnull` is dead and the
                // fall-through is always taken. Skip both the TEST
                // and the JE. Not at a merge point: there the tested value
                // may have been pushed on another path than the `aload`
                // textually before this PC.
                let proven_nonnull = !self.null_check_info.is_merge_point(pc)
                    && preceding_aload_nonnull_local(code, pc)
                        .is_some_and(|l| self.is_local_nonnull(pc, l));
                if proven_nonnull {
                    // No-op: fall through. We still need a non-empty
                    // branch-target record so downstream merges see
                    // the expected stack depth.
                    self.record_branch_target_depth(target_pc);
                    self.reset_spills();
                    pc += 3;
                } else {
                    self.load_slot_to_reg(RCX, slot);
                    // TEST RCX, RCX (REX.W + 0x85 /r)
                    self.rex_w();
                    self.buf.emit(&[0x85, 0xC9]); // TEST RCX, RCX

                    // JE rel32 (jump if null / zero)
                    self.buf.emit_byte(0x0F);
                    self.buf.emit_byte(0x84); // JE
                    let patch_offset = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                    self.forward_patches.push((patch_offset, target_pc));
                    self.record_branch_target_depth(target_pc);
                    self.reset_spills();
                    pc += 3;
                }
            }

            // ifnonnull (0xc7) — branch if reference is not null
            0xc7 => {
                self.flush_scratch_registers();
                let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32; // Widening: always safe
                let target_pc = match pc.checked_add_signed(offset as isize) {
                    // Cast: address arithmetic
                    Some(t) => t,
                    None => return WalkStep::Return( false), // invalid branch target
                };
                if target_pc <= pc {
                    self.emit_safepoint_poll();
                }

                // Canonicalize for forward merge points BEFORE popping the
                // operand (see ifeq..ifle / ifnull above).
                if target_pc > pc && self.stack.len() > 1 {
                    self.canonicalize_stack();
                }
                let slot = self.pop_stack();
                // HIGH-1 / Fix 1 — null-check elimination. If the
                // tested value is proven non-null, `ifnonnull` is
                // always taken: emit an unconditional JMP rel32 and
                // skip the TEST + Jcc pair. Saves the 3-byte TEST
                // + 1-byte (Jcc opcode-pair high byte) for every
                // proven site. Not at a merge point (see `ifnull`).
                let proven_nonnull = !self.null_check_info.is_merge_point(pc)
                    && preceding_aload_nonnull_local(code, pc)
                        .is_some_and(|l| self.is_local_nonnull(pc, l));
                if proven_nonnull {
                    // JMP rel32 (5 bytes; patched).
                    self.buf.emit_byte(0xE9);
                    let patch_offset = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    self.forward_patches.push((patch_offset, target_pc));
                    self.record_branch_target_depth(target_pc);
                    self.reset_spills();
                    pc += 3;
                } else {
                    self.load_slot_to_reg(RCX, slot);
                    // TEST RCX, RCX (REX.W + 0x85 /r)
                    self.rex_w();
                    self.buf.emit(&[0x85, 0xC9]); // TEST RCX, RCX

                    // JNE rel32 (jump if not null / non-zero)
                    self.buf.emit_byte(0x0F);
                    self.buf.emit_byte(0x85); // JNE
                    let patch_offset = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                    self.forward_patches.push((patch_offset, target_pc));
                    self.record_branch_target_depth(target_pc);
                    self.reset_spills();
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
