// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Constants, local variables, `iinc`, `wide` and the operand-stack bytecodes in the single-pass backend's bytecode walk.
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
    pub(super) fn walk_local_stack(
        &mut self,
        code: &[u8],
        code_len: usize,
        op: u8,
        mut pc: usize,
        _dead: &mut bool,
        branch_targets: &[bool],
    ) -> WalkStep {
        match op {
            // nop
            0x00 => {
                pc += 1;
            }

            // aconst_null — push 0 (null reference)
            0x01 => {
                self.emit_xor_reg_self(RAX);
                self.push_from_rax();
                // T1.1.a — null is a valid object reference per JVMS.
                self.mark_top_as_oop();
                pc += 1;
            }

            // iconst_m1..iconst_5
            0x02..=0x08 => {
                let val = op as i32 - 3; // Widening: always safe
                if self.try_const_arith_peephole(val, pc + 1, code, code_len, &branch_targets) {
                    pc += 2;
                } else if let Some(next_pc) = self.try_const_compare_peephole(
                    val,
                    pc + 1,
                    code,
                    code_len,
                    &branch_targets,
                ) {
                    pc = next_pc;
                } else {
                    self.emit_mov_imm32_sx(RAX, val);
                    self.push_from_rax();
                    pc += 1;
                }
            }

            // lconst_0
            0x09 => {
                self.emit_xor_reg_self(RAX);
                self.push_from_rax();
                pc += 1;
            }

            // lconst_1
            0x0a => {
                self.emit_mov_imm32_sx(RAX, 1);
                self.push_from_rax();
                pc += 1;
            }

            // fconst_0
            0x0b => {
                // 0.0f32 → bits = 0x00000000
                self.emit_xor_reg_self(RAX);
                self.push_from_rax();
                pc += 1;
            }

            // fconst_1
            0x0c => {
                // 1.0f32 → bits = 0x3F800000 = 1065353216
                self.emit_mov_imm32_sx(RAX, 0x3F80_0000u32 as i32); // Cast: x86-64 immediate encoding
                self.push_from_rax();
                pc += 1;
            }

            // fconst_2
            0x0d => {
                // 2.0f32 → bits = 0x40000000 = 1073741824
                self.emit_mov_imm32_sx(RAX, 0x4000_0000u32 as i32); // Cast: x86-64 immediate encoding
                self.push_from_rax();
                pc += 1;
            }

            // dconst_0
            0x0e => {
                // 0.0f64 → bits = 0x0000000000000000
                self.emit_xor_reg_self(RAX);
                self.push_from_rax();
                pc += 1;
            }

            // dconst_1
            0x0f => {
                // 1.0f64 → bits = 0x3FF0000000000000
                self.emit_mov_imm64(RAX, 0x3FF0_0000_0000_0000u64 as i64); // Cast: JIT ABI convention
                self.push_from_rax();
                pc += 1;
            }

            // bipush
            0x10 => {
                let val = code[pc + 1] as i8 as i32; // Widening: always safe
                if self.try_const_arith_peephole(val, pc + 2, code, code_len, &branch_targets) {
                    pc += 3;
                } else if let Some(next_pc) = self.try_const_compare_peephole(
                    val,
                    pc + 2,
                    code,
                    code_len,
                    &branch_targets,
                ) {
                    pc = next_pc;
                } else {
                    self.emit_mov_imm32_sx(RAX, val);
                    self.push_from_rax();
                    pc += 2;
                }
            }

            // sipush
            0x11 => {
                let val = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32; // Widening: always safe
                if self.try_const_arith_peephole(val, pc + 3, code, code_len, &branch_targets) {
                    pc += 4;
                } else if let Some(next_pc) = self.try_const_compare_peephole(
                    val,
                    pc + 3,
                    code,
                    code_len,
                    &branch_targets,
                ) {
                    pc = next_pc;
                } else {
                    self.emit_mov_imm32_sx(RAX, val);
                    self.push_from_rax();
                    pc += 3;
                }
            }

            // ldc — load int/float/string/class constant from CP (1-byte index)
            0x12 => {
                if self.emit_ldc_class(pc) {
                    pc += 2;
                    return WalkStep::Next(pc);
                }
                if self.ldc_class_info_idx.contains_key(&pc) {
                    return WalkStep::Return( false);
                }
                if self.emit_ldc_string(pc) {
                    pc += 2;
                    return WalkStep::Next(pc);
                }
                if self.ldc_string_info_idx.contains_key(&pc) {
                    // Recognised as a string `ldc` and NOT emittable —
                    // `ldc_string_cp` unwired, or no context slot. Bail the
                    // site rather than fall through to `ldc_info`, which
                    // does not hold this pc and would push a null.
                    return WalkStep::Return( false);
                }
                let val = self.ldc_info_idx.get(&pc).map(|&i| self.ldc_info[i].1);
                match val {
                    Some(v) => {
                        self.emit_mov_imm64(RAX, v);
                        self.push_from_rax();
                        pc += 2;
                    }
                    None => return WalkStep::Return( false),
                }
            }

            // ldc_w — load int/float/string/class constant from CP (2-byte index)
            0x13 => {
                if self.emit_ldc_class(pc) {
                    pc += 3;
                    return WalkStep::Next(pc);
                }
                if self.ldc_class_info_idx.contains_key(&pc) {
                    return WalkStep::Return( false);
                }
                if self.emit_ldc_string(pc) {
                    pc += 3;
                    return WalkStep::Next(pc);
                }
                if self.ldc_string_info_idx.contains_key(&pc) {
                    // Recognised as a string `ldc` and NOT emittable —
                    // `ldc_string_cp` unwired, or no context slot. Bail the
                    // site rather than fall through to `ldc_info`, which
                    // does not hold this pc and would push a null.
                    return WalkStep::Return( false);
                }
                let val = self.ldc_info_idx.get(&pc).map(|&i| self.ldc_info[i].1);
                match val {
                    Some(v) => {
                        self.emit_mov_imm64(RAX, v);
                        self.push_from_rax();
                        pc += 3;
                    }
                    None => return WalkStep::Return( false),
                }
            }

            // ldc2_w — load long/double constant from CP (resolved to i64)
            0x14 => {
                // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                let val = self.ldc2w_info_idx.get(&pc).map(|&i| self.ldc2w_info[i].1);
                match val {
                    Some(v) => {
                        // Long const-arith fusion (perf/halfgap residuals,
                        // 2026-07-18): `ldc2_w K; l{mul,div,rem,add,sub}` is
                        // the dominant shape of long arithmetic kernels
                        // (`i * 3`, `i / 2`, `i % 7`). A long op as the next
                        // opcode implies the constant is a long, not a
                        // double (the verifier rejects the mix).
                        if self.try_const_arith_peephole_long(
                            v,
                            pc + 3,
                            code,
                            code_len,
                            &branch_targets,
                        ) {
                            pc += 4;
                            return WalkStep::Next(pc);
                        }
                        self.emit_mov_imm64(RAX, v);
                        self.push_from_rax();
                        pc += 3;
                    }
                    None => {
                        // Not resolved — bail out; method will stay interpreted
                        return WalkStep::Return( false);
                    }
                }
            }

            // iload / lload / fload / dload / aload
            // iload/lload/fload/dload/aload (wide index: opcode 0x15-0x19, then idx byte)
            0x15..=0x19 => {
                // Affine self-recurrence strength reduction (CRATONVM_JIT_REASSOC).
                if op == 0x15 && crate::ir_optimize::reassoc_enabled() {
                    if let Some((local, k, c, end)) =
                        match_affine_chain(code, pc, code_len, &branch_targets)
                    {
                        self.emit_affine_fold(local, k, c);
                        pc = end;
                        return WalkStep::Next(pc);
                    }
                }
                let idx = code[pc + 1] as usize; // Widening: always safe
                                                 // fload (0x17) and dload (0x18) may have XMM-allocated locals
                if matches!(op, 0x17 | 0x18) {
                    if let Some(xmm) = self.xmm_for_local(idx) {
                        // FP value — never an oop.
                        self.stack_push(StackSlot::Xmm(xmm), false);
                        pc += 2;
                        return WalkStep::Next(pc);
                    }
                }
                let is_aload = op == 0x19;
                if let Some(local_reg) = self.reg_for_local(idx) {
                    // T1.1.a — only aload pushes oops; iload/lload/fload/dload
                    // push primitives. Stage 1 — keep marks in lockstep.
                    self.stack_push(StackSlot::CalleeSaved(local_reg), is_aload);
                } else {
                    let off = self.local_offset(idx);
                    self.emit_load_local(RAX, off);
                    self.push_from_rax();
                    if is_aload {
                        self.mark_top_as_oop();
                    }
                }
                pc += 2;
            }

            // iload_0..iload_3
            0x1a..=0x1d => {
                // Affine self-recurrence strength reduction (CRATONVM_JIT_REASSOC):
                // fold a run of `x = x*c1 + c2` steps into one `x = x*K + C`.
                if crate::ir_optimize::reassoc_enabled() {
                    if let Some((local, k, c, end)) =
                        match_affine_chain(code, pc, code_len, &branch_targets)
                    {
                        self.emit_affine_fold(local, k, c);
                        pc = end;
                        return WalkStep::Next(pc);
                    }
                }
                let idx = (op - 0x1a) as usize; // Widening: always safe
                if let Some(local_reg) = self.reg_for_local(idx) {
                    // Zero-cost: just record register reference on simulated
                    // stack. iload pushes a primitive — never an oop.
                    self.stack_push(StackSlot::CalleeSaved(local_reg), false);
                } else {
                    let off = self.local_offset(idx);
                    self.emit_load_local(RAX, off);
                    self.push_from_rax();
                }
                pc += 1;
            }

            // lload_0..lload_3
            0x1e..=0x21 => {
                let idx = (op - 0x1e) as usize; // Widening: always safe
                if let Some(local_reg) = self.reg_for_local(idx) {
                    self.stack_push(StackSlot::CalleeSaved(local_reg), false);
                } else {
                    let off = self.local_offset(idx);
                    self.emit_load_local(RAX, off);
                    self.push_from_rax();
                }
                pc += 1;
            }

            // fload_0..fload_3 (float load)
            0x22..=0x25 => {
                let idx = (op - 0x22) as usize; // Widening: always safe
                if let Some(xmm) = self.xmm_for_local(idx) {
                    self.stack_push(StackSlot::Xmm(xmm), false);
                } else if let Some(local_reg) = self.reg_for_local(idx) {
                    self.stack_push(StackSlot::CalleeSaved(local_reg), false);
                } else {
                    let off = self.local_offset(idx);
                    self.emit_load_local(RAX, off);
                    self.push_from_rax();
                }
                pc += 1;
            }

            // dload_0..dload_3 (double load)
            0x26..=0x29 => {
                let idx = (op - 0x26) as usize; // Widening: always safe
                if let Some(xmm) = self.xmm_for_local(idx) {
                    // Zero-cost push: just reference the XMM register.
                    // No code emitted until the value is consumed.
                    self.stack_push(StackSlot::Xmm(xmm), false);
                } else if let Some(local_reg) = self.reg_for_local(idx) {
                    self.stack_push(StackSlot::CalleeSaved(local_reg), false);
                } else {
                    let off = self.local_offset(idx);
                    self.emit_load_local(RAX, off);
                    self.push_from_rax();
                }
                pc += 1;
            }

            // aload_0..aload_3 (reference load — identical to iload for JIT)
            0x2a..=0x2d => {
                let idx = (op - 0x2a) as usize; // Widening: always safe
                if let Some(local_reg) = self.reg_for_local(idx) {
                    // aload* always pushes an object ref. Stage 1 — keep
                    // marks in lockstep and tag the entry. CalleeSaved slots
                    // don't have a frame offset, so the oop-map walker skips
                    // them (preserved by the ABI across calls and cached by
                    // the JIT's frame save/restore prologue).
                    self.stack_push(StackSlot::CalleeSaved(local_reg), true);
                } else {
                    let off = self.local_offset(idx);
                    self.emit_load_local(RAX, off);
                    self.push_from_rax();
                    // T1.1.a — aload* always pushes an object ref.
                    self.mark_top_as_oop();
                }
                pc += 1;
            }

            // istore / lstore / fstore / dstore / astore
            // istore/lstore/fstore/dstore/astore (wide index)
            0x36..=0x3a => {
                let idx = code[pc + 1] as usize; // Widening: always safe
                                                 // fstore (0x38) and dstore (0x39) may have XMM-allocated locals
                if matches!(op, 0x38 | 0x39) {
                    if let Some(dst_xmm) = self.xmm_for_local(idx) {
                        let slot = self.pop_stack();
                        match slot {
                            StackSlot::Xmm(src) if src == dst_xmm => {}
                            StackSlot::Xmm(src) => {
                                if op == 0x39 {
                                    self.emit_movsd_xmm_xmm(dst_xmm, src);
                                } else {
                                    self.emit_movss_xmm_xmm(dst_xmm, src);
                                }
                            }
                            _ => {
                                self.load_slot_to_reg(RAX, slot);
                                self.emit_movq_xmm_from_rax(dst_xmm);
                            }
                        }
                        pc += 2;
                        return WalkStep::Next(pc);
                    }
                }
                self.pop_to_rax();
                if let Some(local_reg) = self.reg_for_local(idx) {
                    self.invalidate_callee_saved(local_reg);
                    self.emit_mov_reg_reg(local_reg, RAX);
                } else {
                    let off = self.local_offset(idx);
                    self.emit_store_local(off, RAX);
                }
                pc += 2;
            }

            // istore_0..istore_3
            0x3b..=0x3e => {
                let idx = (op - 0x3b) as usize; // Widening: always safe
                if let Some(local_reg) = self.reg_for_local(idx) {
                    self.invalidate_callee_saved(local_reg);
                }
                self.pop_to_rax();
                if let Some(local_reg) = self.reg_for_local(idx) {
                    self.emit_mov_reg_reg(local_reg, RAX);
                } else {
                    let off = self.local_offset(idx);
                    self.emit_store_local(off, RAX);
                }
                pc += 1;
            }

            // lstore_0..lstore_3
            0x3f..=0x42 => {
                let idx = (op - 0x3f) as usize; // Widening: always safe
                if let Some(local_reg) = self.reg_for_local(idx) {
                    self.invalidate_callee_saved(local_reg);
                }
                self.pop_to_rax();
                if let Some(local_reg) = self.reg_for_local(idx) {
                    self.emit_mov_reg_reg(local_reg, RAX);
                } else {
                    let off = self.local_offset(idx);
                    self.emit_store_local(off, RAX);
                }
                pc += 1;
            }

            // fstore_0..fstore_3 (float store)
            0x43..=0x46 => {
                let idx = (op - 0x43) as usize; // Widening: always safe
                if let Some(dst_xmm) = self.xmm_for_local(idx) {
                    let slot = self.pop_stack();
                    match slot {
                        StackSlot::Xmm(src) if src == dst_xmm => {}
                        StackSlot::Xmm(src) => {
                            self.emit_movss_xmm_xmm(dst_xmm, src);
                        }
                        _ => {
                            self.load_slot_to_reg(RAX, slot);
                            self.emit_movq_xmm_from_rax(dst_xmm);
                        }
                    }
                } else {
                    self.pop_to_rax();
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        self.invalidate_callee_saved(local_reg);
                        self.emit_mov_reg_reg(local_reg, RAX);
                    } else {
                        let off = self.local_offset(idx);
                        self.emit_store_local(off, RAX);
                    }
                }
                pc += 1;
            }

            // dstore_0..dstore_3 (double store)
            0x47..=0x4a => {
                let idx = (op - 0x47) as usize; // Widening: always safe
                                                // Optimize: if top-of-stack is Xmm and target is XMM local,
                                                // move directly XMM→XMM without going through RAX.
                if let Some(dst_xmm) = self.xmm_for_local(idx) {
                    let slot = self.pop_stack();
                    match slot {
                        StackSlot::Xmm(src) if src == dst_xmm => {
                            // Already in the right register — no-op
                        }
                        StackSlot::Xmm(src) => {
                            self.emit_movsd_xmm_xmm(dst_xmm, src);
                        }
                        _ => {
                            self.load_slot_to_reg(RAX, slot);
                            self.emit_movq_xmm_from_rax(dst_xmm);
                        }
                    }
                } else {
                    self.pop_to_rax();
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        self.invalidate_callee_saved(local_reg);
                        self.emit_mov_reg_reg(local_reg, RAX);
                    } else {
                        let off = self.local_offset(idx);
                        self.emit_store_local(off, RAX);
                    }
                }
                pc += 1;
            }

            // astore_0..astore_3 (reference store — identical to istore for JIT)
            0x4b..=0x4e => {
                let idx = (op - 0x4b) as usize; // Widening: always safe
                if let Some(local_reg) = self.reg_for_local(idx) {
                    self.invalidate_callee_saved(local_reg);
                }
                self.pop_to_rax();
                if let Some(local_reg) = self.reg_for_local(idx) {
                    self.emit_mov_reg_reg(local_reg, RAX);
                } else {
                    let off = self.local_offset(idx);
                    self.emit_store_local(off, RAX);
                }
                pc += 1;
            }

            // pop
            0x57 => {
                let _ = self.pop_stack();
                pc += 1;
            }

            // pop2
            //
            // Absent until 2026-08-17, and not a rare shape: javac emits
            // `pop2` whenever a call returning `long`/`double` is used as a
            // STATEMENT. commons-math's `PSquarePercentile$Markers` has one
            // in `adjustHeightsOfMarkers` (discarding `estimate(int)`) and
            // one in `findCellAndUpdateMinMax` (discarding a synthetic
            // `access$502` setter), and those two methods are called once
            // per `increment()` — so the whole P-square hot loop refused to
            // compile over a missing stack-height adjustment. See
            // bug-commonsmath-accuratemathtest-psquarepercentiletest-interpreter-throughput-cliff-20260816.
            //
            // The operand model holds ONE entry per VALUE, not per JVM slot,
            // so form 2 (a single category-2 value) pops once and form 1
            // (two category-1 values) pops twice. `dup2_top_cat2` answers
            // the same width question `dup2` asks, from the producing
            // instruction.
            0x58 => {
                match self.dup2_top_cat2(code, pc) {
                    // FORM-2: one category-2 value occupies one entry.
                    Some(true) => {
                        let _ = self.pop_stack();
                    }
                    // FORM-1: two category-1 values.
                    Some(false) if self.stack.len() >= 2 => {
                        let _ = self.pop_stack();
                        let _ = self.pop_stack();
                    }
                    _ => {
                        // Unprovable top width (or a malformed FORM-1 with
                        // height < 2). Mirror `dup2`: keep the modelled
                        // height plausible for the rest of the dispatch loop
                        // so a later handler does not raise a second,
                        // misleading failure before the post-loop `failed`
                        // check discards this compilation.
                        self.fail("singlepass-codegen/pop2-unprovable-top-width");
                        for _ in 0..2 {
                            if self.stack.is_empty() {
                                break;
                            }
                            let _ = self.pop_stack();
                        }
                    }
                }
                pc += 1;
            }

            // dup
            0x59 => {
                let top = self.peek_stack();
                // T1.1.a / EC oop-map fix: capture the source slot's oop
                // mark so the duplicate carries it. The `self.stack.push`
                // fast-paths below would otherwise push to `self.stack`
                // WITHOUT a paired `stack_oop_marks` push (desyncing the
                // two vectors), and the `push_from_rax`/`push_stack` paths
                // push a hard-coded `false` — both leave a duplicated
                // object reference UNMARKED in the precise oop map. A
                // duplicated oop live across a safepoint must stay precisely
                // mapped so a moving GC remaps it; otherwise it can decay to
                // a stale/garbage base.
                let top_is_oop = self.stack_oop_marks.last().copied().unwrap_or(false);
                match top {
                    StackSlot::Frame(off) => {
                        self.emit_load_local(RAX, off);
                        self.push_from_rax();
                    }
                    StackSlot::CalleeSaved(_) => {
                        // Zero-cost: just duplicate the register reference
                        self.stack.push(top);
                        self.stack_oop_marks.push(top_is_oop);
                    }
                    StackSlot::Xmm(_) => {
                        // Zero-cost: just duplicate the XMM register reference
                        self.stack.push(top);
                        self.stack_oop_marks.push(top_is_oop);
                    }
                    StackSlot::Scratch(reg, ..) => {
                        // Scratch register holds the value — try to dup into
                        // another scratch register, else spill original to frame
                        // and push another frame copy.
                        //
                        let avail = SCRATCH_REGS.iter().copied().find(|&sr| {
                            sr != reg
                                && !self
                                    .stack
                                    .iter()
                                    .any(|s| matches!(s, StackSlot::Scratch(r, ..) if *r == sr))
                        });
                        if let Some(sr) = avail {
                            self.emit_mov_reg_reg(sr, reg);
                            self.stack.push(StackSlot::Scratch(sr));
                            self.stack_oop_marks.push(top_is_oop);
                        } else {
                            // No scratch available — load to RAX and push via frame
                            self.emit_mov_reg_reg(RAX, reg);
                            if let Some(StackSlot::Frame(off)) = self.push_stack() {
                                self.emit_store_local(off, RAX);
                            }
                        }
                    }
                }
                // Propagate the oop mark onto the freshly-pushed duplicate
                // (the `push_from_rax`/`push_stack` paths pushed `false`).
                if top_is_oop {
                    if let Some(m) = self.stack_oop_marks.last_mut() {
                        *m = true;
                    }
                }
                pc += 1;
            }

            // dup_x1 — `[…, b, a] → […, a, b, a]`. JVMS §6.5 guarantees both
            // operands are category-1, so unlike dup2/dup_x2 no width proof
            // is needed. javac emits this for a field post-increment used as
            // a value (`xBuf[xBufOff++] = in` in BC's GeneralDigest.update —
            // the per-byte digest hot path that kept every BC digest
            // interpreter-bound while this opcode bailed).
            //
            // Implementation: materialize ONE copy of the top value into a
            // fresh frame slot (fresh offsets only grow, so no aliasing with
            // the live `b`/`a` slots), then rotate the top three MODEL
            // entries so the copy sits below the original pair. Only the
            // copy costs instructions. The rotated entries' frame offsets
            // are momentarily non-canonical, which is fine:
            // `canonicalize_stack` resolves arbitrary offset permutations as
            // a parallel-move problem at the next branch/call boundary.
            0x5a => {
                if dupx_codegen_disabled() || dup_x1_codegen_disabled() || self.stack.len() < 2
                {
                    self.fail("singlepass-codegen/dup_x1-unsupported-shape");
                    let _ = self.push_stack();
                } else {
                    let a_slot = self.peek_stack();
                    let a_oop = self.stack_oop_marks.last().copied().unwrap_or(false);
                    let before = self.stack.len();
                    self.load_slot_to_reg(RAX, a_slot);
                    self.push_from_rax(); // […, b, a, aC]
                                          // `push_from_rax` is silent when `push_stack` cannot
                                          // reserve a spill slot: it emits nothing and does NOT
                                          // grow the model. The rotate below indexes `n - 3`, so
                                          // a missed push rotates the WRONG three entries and
                                          // leaves the operand stack one short — silent wrong
                                          // code rather than a bail.
                    if self.stack.len() != before + 1 {
                        self.fail("singlepass-codegen/dup_x1-copy-not-pushed");
                        pc += 1;
                        return WalkStep::Next(pc);
                    }
                    if a_oop {
                        self.mark_top_as_oop();
                    }
                    let n = self.stack.len();
                    if dupx_trace() {
                        eprintln!(
                            "[DUPX1-TRACE] {} pc={} before={:?} marks={:?}",
                            self.method_key,
                            pc,
                            &self.stack[n - 3..],
                            &self.stack_oop_marks[n - 3..]
                        );
                    }
                    self.stack[n - 3..].rotate_right(1); // […, aC, b, a]
                    self.stack_oop_marks[n - 3..].rotate_right(1);
                    if dupx_trace() {
                        eprintln!(
                            "[DUPX1-TRACE] {} pc={} after ={:?} marks={:?}",
                            self.method_key,
                            pc,
                            &self.stack[n - 3..],
                            &self.stack_oop_marks[n - 3..]
                        );
                    }
                    if dupx_eager_canon() {
                        self.canonicalize_stack();
                    }
                }
                pc += 1;
            }

            // dup_x2 — FORM-1 `[…, c, b, a] → […, a, c, b, a]` (all three
            // category-1) vs FORM-2 `[…, w, a] → […, a, w, a]` (w is a
            // category-2 long/double = ONE slot in this model). The top is
            // category-1 in BOTH forms; what decides the shape is the width
            // of the entry BELOW it — two entries deep for FORM-2, three for
            // FORM-1 — and that width is exactly what this backend's compact
            // operand model does not carry.
            //
            // Two independent witnesses answer it, and either alone suffices:
            //
            //   * `stack_entry_categories` — the `x64::stack_kinds` forward
            //     analysis, admitted only when its depth and per-entry
            //     ref-ness agree with the emitter's own model and, for the
            //     top entry, with `dup2_top_cat2`'s wholly independent
            //     peephole. This is the same second-entry oracle `dup2_x2`
            //     (0x5e) already uses, and it is what lifts the restriction
            //     this arm used to carry.
            //   * the NEXT opcode is a category-1 array store — then the
            //     verifier guarantees the top three slots are
            //     `[arrayref, index, cat1-value]`, i.e. FORM-1. Kept as the
            //     fallback for methods whose kind analysis poisons: it is
            //     javac's `++z[i]` / `--z[i]` value-producing pattern, which
            //     is BC's `Nat.inc`/`Nat.dec` DRBG block-counter helpers
            //     re-running the whole compile pipeline 35 923× in one
            //     crypto-prng suite run.
            //
            // What the analysis adds over the peephole is the POST-increment
            // idiom `z[i]++` / `arr[n[0]++] = v`, where the `dup_x2` is
            // followed by `iconst_1; iadd; iastore` rather than by the store
            // itself. That is the shape that kept
            // `HibfixComposeProbe2.chain` `ineligible-by-policy` at pc=15 —
            // see
            // `completablefuture-composition-force-interpreted-by-a-stale-forkjointask-blocklist-FIXED-20260827.md`.
            0x5b => {
                let cats = self.stack_entry_categories(pc);
                let peephole_top = self.dup2_top_cat2(code, pc);
                // Insertion depth in MODEL entries, or `None` for a form
                // this compile cannot prove.
                let depth = cats
                    .as_ref()
                    .and_then(|cats| {
                        let n = cats.len();
                        let top = (*cats.get(n.checked_sub(1)?)?)?;
                        // Third opinion: when the peephole answers for the
                        // top it must agree. A disagreement means one of two
                        // independent analyses is wrong; use neither.
                        if matches!(peephole_top, Some(p) if p != top) {
                            return None;
                        }
                        if top {
                            // No legal `dup_x2` form has a category-2 top.
                            return None;
                        }
                        let second = (*cats.get(n.checked_sub(2)?)?)?;
                        if second {
                            Some(2usize) // FORM-2
                        } else {
                            // FORM-1 additionally requires v3 category-1.
                            let third = (*cats.get(n.checked_sub(3)?)?)?;
                            if third {
                                None
                            } else {
                                Some(3usize)
                            }
                        }
                    })
                    .or_else(|| {
                        let next_is_cat1_astore = pc + 1 < code_len
                            && matches!(code[pc + 1], 0x4f | 0x51 | 0x53 | 0x54 | 0x55 | 0x56);
                        next_is_cat1_astore.then_some(3usize)
                    });
                let disabled = dupx_codegen_disabled() || dup_x2_codegen_disabled();
                match depth {
                    Some(depth) if !disabled && self.stack.len() >= depth => {
                        let a_slot = self.peek_stack();
                        let a_oop = self.stack_oop_marks.last().copied().unwrap_or(false);
                        let before = self.stack.len();
                        self.load_slot_to_reg(RAX, a_slot);
                        self.push_from_rax(); // […, c, b, a, aC]
                                              // `push_from_rax` is SILENT when it cannot reserve a
                                              // spill slot: it emits nothing and does NOT grow the
                                              // model, and the rotate below would then reorder the
                                              // WRONG entries and leave the operand stack one
                                              // short — silent wrong code rather than a bail. The
                                              // same guard has been in `dup_x1`/`dup2_x1`/
                                              // `dup2_x2` since they were written; this arm was
                                              // the one missing it.
                        if self.stack.len() != before + 1 {
                            self.fail("singlepass-codegen/dup_x2-copy-not-pushed");
                            pc += 1;
                            return WalkStep::Next(pc);
                        }
                        if a_oop {
                            self.mark_top_as_oop();
                        }
                        let n = self.stack.len();
                        let window = depth + 1;
                        self.stack[n - window..].rotate_right(1); // […, aC, c, b, a]
                        self.stack_oop_marks[n - window..].rotate_right(1);
                        if dupx_eager_canon() {
                            self.canonicalize_stack();
                        }
                    }
                    _ => {
                        // Unprovable form (or the kill switch) — stay
                        // interpreted; placeholder keeps the model height
                        // plausible until the post-loop `failed` check.
                        self.fail("singlepass-codegen/dup_x2-unprovable-form");
                        let _ = self.push_stack();
                    }
                }
                pc += 1;
            }

            // dup2 — FORM-1 (`[…, a, b] → […, a, b, a, b]`, two category-1
            // values) or FORM-2 (`[…, w] → […, w, w]`, one category-2
            // long/double = a single slot in this model). The form is
            // decided by the top operand's width = the result width of the
            // instruction producing it (`dup2_top_cat2`, which reads the
            // immediately-preceding op + resolved field/invoke metadata).
            // FORM-2 is exactly `dup` of the one slot. If the width can't be
            // proven locally, bail to the interpreter rather than risk a
            // category miscompile (the historic FORM-1-on-cat-2 hard abort).
            // This is the codegen-side replacement for the
            // `dup2_category_safe` reject gate, which over-rejected FORM-1
            // methods (regressing bintrees18) because the CP-less scan could
            // not resolve a `<getfield/invoke>; dup2` top to category-1.
            0x5c => {
                match self.dup2_top_cat2(code, pc) {
                    Some(true) => {
                        // FORM-2: duplicate the single category-2 top slot.
                        self.emit_dup_top_slot();
                    }
                    Some(false) if self.stack.len() >= 2 => {
                        // FORM-1: duplicate the top two (category-1) slots.
                        let len = self.stack.len();
                        let a = self.stack[len - 2]; // deeper
                        let b = self.stack[len - 1]; // top
                                                     // EC oop-map fix: carry the two source oop marks onto
                                                     // the two duplicated entries (push_from_rax pushes `false`).
                        let ml = self.stack_oop_marks.len();
                        let a_oop = self
                            .stack_oop_marks
                            .get(ml.wrapping_sub(2))
                            .copied()
                            .unwrap_or(false);
                        let b_oop = self
                            .stack_oop_marks
                            .get(ml.wrapping_sub(1))
                            .copied()
                            .unwrap_or(false);
                        self.load_slot_to_reg(RAX, a);
                        self.push_from_rax();
                        if a_oop {
                            if let Some(m) = self.stack_oop_marks.last_mut() {
                                *m = true;
                            }
                        }
                        self.load_slot_to_reg(RAX, b);
                        self.push_from_rax();
                        if b_oop {
                            if let Some(m) = self.stack_oop_marks.last_mut() {
                                *m = true;
                            }
                        }
                    }
                    _ => {
                        // Unprovable top width (or a malformed FORM-1 with
                        // height < 2) — stay interpreted. Push two
                        // placeholders so downstream opcode handlers keep a
                        // plausible stack height until the post-loop `failed`
                        // check discards this compilation.
                        self.fail("singlepass-codegen/dup2-unprovable-top-width");
                        let _ = self.push_stack();
                        let _ = self.push_stack();
                    }
                }
                pc += 1;
            }

            // dup2_x1 — FORM-2 `[…, b, w] → […, w, b, w]` (w category-2 =
            // ONE entry in this value model, b category-1) vs FORM-1
            // `[…, c, b, a] → […, b, a, c, b, a]` (all three category-1).
            //
            // Both are lowered now, and each has its own witness:
            //
            //   * FORM-2 needs only the TOP's width. A VERIFIED `dup2_x1`
            //     whose top is category-2 cannot be FORM-1 (that form is all
            //     category-1), and JVMS requires its value2 to be
            //     category-1 — a category-2 second operand would have had to
            //     be `dup2_x2`. So `dup2_top_cat2` answering `true` settles
            //     the shape on its own, and that peephole is kept as the
            //     fallback for methods whose kind analysis poisons.
            //   * FORM-1 duplicates TWO entries and so needs the widths of
            //     the three entries under the dup — `stack_entry_categories`
            //     (the `x64::stack_kinds` forward analysis, cross-checked
            //     against the emitter's depth, its per-entry oop marks and
            //     the peephole's opinion of the top). Same oracle, same
            //     admission rules, as `dup_x2` and `dup2_x2`.
            //
            // What FORM-2 unsticks: javac emits `dup2_x1` for
            // `return this.field = value;` on a long/double field, which is
            // every synthetic outer-class setter of a `double` field.
            // commons-math's `PSquarePercentile$Marker.access$502` is one,
            // reached from the P-square min/max update path. FORM-1 is the
            // same statement over a category-1 field, and the `map[k] = v`
            // shapes that leave `[map, key, value]` on the stack.
            0x5d => {
                let cats = self.stack_entry_categories(pc);
                let peephole_top = self.dup2_top_cat2(code, pc);
                // (entries duplicated, insertion depth in entries).
                let shape = cats
                    .as_ref()
                    .and_then(|cats| {
                        let n = cats.len();
                        let top = (*cats.get(n.checked_sub(1)?)?)?;
                        // Third opinion, as in `dup_x2`/`dup2_x2`.
                        if matches!(peephole_top, Some(p) if p != top) {
                            return None;
                        }
                        let second = (*cats.get(n.checked_sub(2)?)?)?;
                        if top {
                            // FORM-2 — JVMS requires value2 category-1.
                            if second {
                                None
                            } else {
                                Some((1usize, 2usize))
                            }
                        } else {
                            // FORM-1 — all three category-1.
                            if second {
                                return None;
                            }
                            let third = (*cats.get(n.checked_sub(3)?)?)?;
                            if third {
                                None
                            } else {
                                Some((2usize, 3usize))
                            }
                        }
                    })
                    .or_else(|| {
                        (peephole_top == Some(true) && self.stack.len() >= 2)
                            .then_some((1usize, 2usize))
                    });
                let disabled = dupx_codegen_disabled() || dup_x1_codegen_disabled();
                match shape {
                    Some((dup_entries, depth)) if !disabled && self.stack.len() >= depth => {
                        // Same emit as `dup2_x2`: materialize the copies into
                        // fresh frame slots deepest-first so the pushed group
                        // ends up in operand order, then rotate the top
                        // `depth + dup_entries` MODEL entries right by
                        // `dup_entries` to slide the copies underneath.
                        let n0 = self.stack.len();
                        let mut ok = true;
                        for k in (0..dup_entries).rev() {
                            let src = self.stack[n0 - 1 - k];
                            let src_oop = self.stack_oop_marks[n0 - 1 - k];
                            let before = self.stack.len();
                            self.load_slot_to_reg(RAX, src);
                            self.push_from_rax();
                            // `push_from_rax` is SILENT when it cannot
                            // reserve a spill slot — it emits nothing and
                            // does not grow the model, and the rotate below
                            // would then reorder the wrong entries.
                            if self.stack.len() != before + 1 {
                                self.fail("singlepass-codegen/dup2_x1-copy-not-pushed");
                                ok = false;
                                break;
                            }
                            if src_oop {
                                self.mark_top_as_oop();
                            }
                        }
                        if ok {
                            let n = self.stack.len();
                            let window = depth + dup_entries;
                            self.stack[n - window..].rotate_right(dup_entries);
                            self.stack_oop_marks[n - window..].rotate_right(dup_entries);
                            if dupx_eager_canon() {
                                self.canonicalize_stack();
                            }
                        }
                    }
                    _ => {
                        self.fail("singlepass-codegen/dup2_x1-unprovable-form");
                        let _ = self.push_stack();
                    }
                }
                pc += 1;
            }

            // dup2_x2 — the last category-dependent stack shuffle x64
            // did not lower. `jit_scan` has always ADMITTED it (it just
            // advances `pc`), so before this arm existed the method reached
            // the dispatch loop's `_ =>` catch-all and lost its compilation
            // for the life of the process, with the refusal attributed to
            // an arm that names nothing. See
            // dup2_x2-is-scan-admitted-but-lowered-by-neither-x64-backend-20260817-FIXED.md.
            //
            // Four JVMS forms. In this backend's operand model — one entry
            // per VALUE, so a category-2 long/double is ONE entry — they
            // are four different shuffles over two, three or four entries:
            //
            //   FORM 4  v1,v2 cat-2   [v2, v1]         -> [v1, v2, v1]
            //   FORM 2  v1 cat-2      [v3, v2, v1]     -> [v1, v3, v2, v1]
            //   FORM 3  v3 cat-2      [v3, v2, v1]     -> [v2, v1, v3, v2, v1]
            //   FORM 1  all cat-1     [v4, v3, v2, v1] -> [v2, v1, v4, v3, v2, v1]
            //
            // So the TOP entry's category decides how many entries are
            // duplicated (one for a cat-2 top, two for a cat-1 pair) and the
            // entry BELOW the duplicated group decides how deep the copy is
            // inserted. `dup2_top_cat2` answers only the first question —
            // which is why this opcode waited for a second-entry oracle.
            // `stack_entry_categories` is it: the widths come from the
            // `x64::stack_kinds` forward analysis, admitted only when its
            // depth and per-entry ref-ness agree with the emitter's own
            // model AND, for the top entry, with `dup2_top_cat2`'s wholly
            // independent peephole answer.
            //
            // aarch64's arm is NOT the template: it pops four operands
            // unconditionally, which is FORM 1 only.
            0x5e => {
                let cats = self.stack_entry_categories(pc);
                let peephole_top = self.dup2_top_cat2(code, pc);
                // Resolve (entries duplicated, insertion depth in entries).
                let shape = cats.as_ref().and_then(|cats| {
                    let n = cats.len();
                    let top = (*cats.get(n.checked_sub(1)?)?)?;
                    // Third opinion: when the peephole answers for the top,
                    // it must agree. A disagreement means one of two
                    // independent analyses is wrong; use neither.
                    if matches!(peephole_top, Some(p) if p != top) {
                        return None;
                    }
                    let second = (*cats.get(n.checked_sub(2)?)?)?;
                    if top {
                        // FORM 4 (second cat-2, two entries) or FORM 2
                        // (second cat-1, three entries).
                        if second {
                            Some((1usize, 2usize))
                        } else {
                            // FORM 2 additionally requires v3 category-1;
                            // verified bytecode guarantees it, and checking
                            // costs one lookup.
                            let third = (*cats.get(n.checked_sub(3)?)?)?;
                            if third {
                                None
                            } else {
                                Some((1, 3))
                            }
                        }
                    } else {
                        // Two cat-1 entries duplicated. v2 is cat-1 in both
                        // remaining forms.
                        if second {
                            return None;
                        }
                        let third = (*cats.get(n.checked_sub(3)?)?)?;
                        if third {
                            Some((2, 3)) // FORM 3
                        } else {
                            // FORM 1 additionally requires v4 category-1.
                            let fourth = (*cats.get(n.checked_sub(4)?)?)?;
                            if fourth {
                                None
                            } else {
                                Some((2, 4))
                            }
                        }
                    }
                });
                let disabled = dupx_codegen_disabled() || dup2_x2_codegen_disabled();
                match shape {
                    Some((dup_entries, depth)) if !disabled => {
                        // Materialize the copies into fresh frame slots
                        // (fresh offsets only grow, so no aliasing with the
                        // live originals), deepest-first so the pushed pair
                        // ends up in operand order, then rotate the top
                        // `depth + dup_entries` MODEL entries right by
                        // `dup_entries` to slide the copies underneath. Only
                        // the copies cost instructions; the rotate is
                        // bookkeeping that `canonicalize_stack` resolves as a
                        // parallel move at the next branch/call boundary.
                        let n0 = self.stack.len();
                        let mut ok = true;
                        for k in (0..dup_entries).rev() {
                            let src = self.stack[n0 - 1 - k];
                            let src_oop = self.stack_oop_marks[n0 - 1 - k];
                            let before = self.stack.len();
                            self.load_slot_to_reg(RAX, src);
                            self.push_from_rax();
                            // `push_from_rax` is SILENT when it cannot
                            // reserve a spill slot: it emits nothing and does
                            // not grow the model, and the rotate below would
                            // then reorder the wrong entries. Same guard as
                            // `dup_x1`/`dup2_x1`.
                            if self.stack.len() != before + 1 {
                                self.fail("singlepass-codegen/dup2_x2-copy-not-pushed");
                                ok = false;
                                break;
                            }
                            if src_oop {
                                self.mark_top_as_oop();
                            }
                        }
                        if ok {
                            let n = self.stack.len();
                            let window = depth + dup_entries;
                            self.stack[n - window..].rotate_right(dup_entries);
                            self.stack_oop_marks[n - window..].rotate_right(dup_entries);
                            if dupx_eager_canon() {
                                self.canonicalize_stack();
                            }
                        }
                    }
                    _ => {
                        // No provable form (or the kill switch) — stay
                        // interpreted. Push two placeholders so downstream
                        // handlers keep a plausible height until the
                        // post-loop `failed` check discards this
                        // compilation, matching `dup2`.
                        self.fail("singlepass-codegen/dup2_x2-unprovable-form");
                        let _ = self.push_stack();
                        let _ = self.push_stack();
                    }
                }
                pc += 1;
            }

            // swap
            0x5f => {
                // EC oop-map fix (round 2): the previous round paired the
                // pushes with `stack_oop_marks` (fixing a desync) but pushed
                // hard-coded `false` on the conservative-frame-sweep argument.
                // That justification only covers `StackSlot::Frame` — the
                // conservative scan walks frame qwords, not registers — so a
                // register-resident oop (`StackSlot::Scratch`/`CalleeSaved`)
                // swapped here lost its precise mark and would not be remapped
                // by a moving GC at the next safepoint; the next deref would
                // read stale from-space. Mirror the existing `dup`/`dup2`
                // mark-propagation: snapshot each operand's mark BEFORE the
                // pops (`pop_stack` discards them) and carry it onto the
                // swapped position so the precise oop map matches the values
                // the slots actually hold.
                let ml = self.stack_oop_marks.len();
                let a_oop = self
                    .stack_oop_marks
                    .get(ml.wrapping_sub(1))
                    .copied()
                    .unwrap_or(false); // top before swap
                let b_oop = self
                    .stack_oop_marks
                    .get(ml.wrapping_sub(2))
                    .copied()
                    .unwrap_or(false); // below-top before swap
                let a = self.pop_stack();
                let b = self.pop_stack();
                match (a, b) {
                    (StackSlot::Frame(off_a), StackSlot::Frame(off_b)) => {
                        // Physically exchange the two frame slots and keep
                        // each slot ENTRY at its original position, so the
                        // positions keep their original (ascending) frame
                        // offsets: below-top stays Frame(off_b) and now
                        // reads value1, top stays Frame(off_a) and reads
                        // value2. The previous code exchanged the memory
                        // but pushed the entries in (a, b) order, which
                        // re-paired each entry with its original value —
                        // the exchange and the reorder cancelled out and
                        // swap was a NO-OP for two frame-resident values.
                        self.emit_load_local(RAX, off_a);
                        self.emit_load_local(RCX, off_b);
                        self.emit_store_local(off_a, RCX);
                        self.emit_store_local(off_b, RAX);
                        self.stack.push(b);
                        self.stack_oop_marks.push(a_oop); // off_b now holds value1
                        self.stack.push(a);
                        self.stack_oop_marks.push(b_oop); // off_a now holds value2
                    }
                    _ => {
                        // Mixed or register-resident — no memory traffic;
                        // reorder the slot entries, each carrying its own
                        // value and oop mark to its new position.
                        self.stack.push(a);
                        self.stack_oop_marks.push(a_oop);
                        self.stack.push(b);
                        self.stack_oop_marks.push(b_oop);
                    }
                }
                // The two pops above may have reclaimed the operands' spill
                // slots (next_spill_offset rewound below the still-live
                // frame slots just pushed back); a later push would then be
                // handed a live slot and clobber it. Recompute the cursor
                // from the live stack.
                self.reset_spills();
                pc += 1;
            }

            // iinc
            0x84 => {
                let idx = code[pc + 1] as usize; // Widening: always safe
                let inc = code[pc + 2] as i8 as i32; // Widening: always safe
                self.emit_iinc_local(idx, inc);
                pc += 3;
            }

            // wide (JVMS §6.5) — the two-byte-index prefix. See `jit_scan`'s
            // own `0xC4` arm for what one un-emittable `iinc_w` cost, and
            // why the scan and this walk are changed together: a form the
            // scan admits and this arm cannot emit is a whole-method
            // compile bail, and a form this arm emits that the scan refuses
            // is unreachable.
            //
            // The load and store bodies mirror the narrow arms above
            // one-for-one — only the operand decode differs — and `iinc`
            // shares its emitter outright rather than keeping a second copy
            // of the ADD/MOVSXD sequence.
            0xc4 => {
                let wop = code[pc + 1];
                // Cast: a JVM local index; `max_locals` bounds it.
                let idx = u16::from_be_bytes([code[pc + 2], code[pc + 3]]) as usize;
                match wop {
                    // wide iload / lload / fload / dload / aload
                    0x15..=0x19 => {
                        let is_aload = wop == 0x19;
                        // fload/dload may have an XMM-allocated local.
                        if matches!(wop, 0x17 | 0x18) {
                            if let Some(xmm) = self.xmm_for_local(idx) {
                                // FP value — never an oop.
                                self.stack_push(StackSlot::Xmm(xmm), false);
                                pc += 4;
                                return WalkStep::Next(pc);
                            }
                        }
                        if let Some(local_reg) = self.reg_for_local(idx) {
                            self.stack_push(StackSlot::CalleeSaved(local_reg), is_aload);
                        } else {
                            let off = self.local_offset(idx);
                            self.emit_load_local(RAX, off);
                            self.push_from_rax();
                            if is_aload {
                                self.mark_top_as_oop();
                            }
                        }
                        pc += 4;
                    }
                    // wide istore / lstore / fstore / dstore / astore
                    0x36..=0x3a => {
                        if matches!(wop, 0x38 | 0x39) {
                            if let Some(dst_xmm) = self.xmm_for_local(idx) {
                                let slot = self.pop_stack();
                                match slot {
                                    StackSlot::Xmm(src) if src == dst_xmm => {}
                                    StackSlot::Xmm(src) => {
                                        if wop == 0x39 {
                                            self.emit_movsd_xmm_xmm(dst_xmm, src);
                                        } else {
                                            self.emit_movss_xmm_xmm(dst_xmm, src);
                                        }
                                    }
                                    _ => {
                                        self.load_slot_to_reg(RAX, slot);
                                        self.emit_movq_xmm_from_rax(dst_xmm);
                                    }
                                }
                                pc += 4;
                                return WalkStep::Next(pc);
                            }
                        }
                        self.pop_to_rax();
                        if let Some(local_reg) = self.reg_for_local(idx) {
                            self.invalidate_callee_saved(local_reg);
                            self.emit_mov_reg_reg(local_reg, RAX);
                        } else {
                            let off = self.local_offset(idx);
                            self.emit_store_local(off, RAX);
                        }
                        pc += 4;
                    }
                    // wide iinc — the SIGNED 16-bit constant is the whole
                    // reason javac emits this prefix in netty's codecs
                    // (`iinc_w 18, -255`).
                    0x84 => {
                        // Widening: i16 -> i32, sign preserved.
                        let inc = i16::from_be_bytes([code[pc + 4], code[pc + 5]]) as i32;
                        self.emit_iinc_local(idx, inc);
                        pc += 6;
                    }
                    // `wide ret` and anything else — `jit_scan` refuses the
                    // same set, so this is unreachable; bail rather than
                    // emit for a form neither walk models.
                    _ => {
                        self.fail("singlepass-codegen/wide-unsupported-opcode");
                        return WalkStep::Return( false);
                    }
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
