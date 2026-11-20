// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Lean fast-path helpers for field barriers, whole-heap card marks,
//! final-target `instanceof`, and entry-hoisted layout-epoch guards.

#[allow(unused_imports)]
use super::*;

/// The BIASED card-table base of a baked whole-heap hull (HotSpot's
/// `byte_map_base`): `map - (hull_lo >> card_shift)`, so the card byte of a
/// mark is `[biased + (mark >> card_shift)]` -- `SHR; MOV R11, imm64; CMP
/// byte [R11 + R10]` with no `SUB` and no hull compare in front of it.
///
/// Used only where the mark is PROVEN inside the hull
/// ([`Lowerer::emit_ir_whole_heap_card_mark_sib_fast`] after its
/// `GC_FLAG_OLD_GEN` receiver test): the Generational heap sets that flag only
/// on objects of its old generation (`OldGen` allocations and promotions, see
/// `JitRefStoreGates::post_skip_mask`), whose card table covers the whole
/// reservation `[base, base + growth_max)` and never re-points it
/// (`CardTable::refresh_jit_card_view`), and `CardTable::with_hull` refuses a
/// hull that does not contain that range. The mark is the receiver or an
/// in-bounds element of it, so `hull_lo <= mark < hull_hi`.
///
/// Why the wrapping arithmetic is exact: `hull_lo` is card-aligned
/// (`with_hull` steps whole summary groups below the card-aligned base), so
/// `(mark >> s) - (hull_lo >> s) == (mark - hull_lo) >> s`, an index in
/// `[0, cards)`. `biased` may wrap (a map below `hull_lo >> s`), but the SIB
/// address `biased + (mark >> s)` is computed modulo 2^64 like the
/// `wrapping_sub` here, so it is exactly `map + index`, a byte of the map.
/// `mark >> s` is below 2^(64 - s) (2^47 for user-space marks), so the 64-bit
/// index register never carries a sign the addressing could misread.
pub(in crate::ir_lower) fn ir_biased_card_base(
    map: usize,
    hull_lo: usize,
    card_shift: u8,
) -> usize {
    map.wrapping_sub(hull_lo >> card_shift)
}

impl<'a> Lowerer<'a> {
    /// Decoupled VM-wide layout-epoch reachability: a passed deopting
    /// `layout_replace_epoch_guard` proves the VM-wide layout epoch for every
    /// dominated block regardless of receiver `NodeId`, subject to the same
    /// OSR-entry and exception-entry checks as [`Self::field_proof_reaches`].
    pub(in crate::ir_lower) fn epoch_proof_reaches(&self, b: usize, block: usize) -> bool {
        if !self.graph.catch_arm_sites.is_empty() && !self.proven_catch_arms_on {
            return false;
        }
        let dom = &self.schedule.dom;
        if !(dom.is_reachable(b) && dom.is_reachable(block) && dom.dominates(b, block)) {
            return false;
        }
        let cached = self
            .field_proof_scope_cache
            .borrow()
            .get(&(b, block))
            .cloned();
        let bypass = match cached {
            Some(v) => v,
            None => {
                let mut bypass: Option<Vec<usize>> = Some(Vec::new());
                for &e in self.field_proof_entry_blocks() {
                    if e == b
                        || !Self::osr_entry_runs_before_entering(self.schedule, e, Some(b))
                            .get(block)
                            .copied()
                            .unwrap_or(true)
                    {
                        continue;
                    }
                    if !self.epoch_proof_entry_is_rechecked_osr_header(e) {
                        bypass = None;
                        break;
                    }
                    if let Some(list) = bypass.as_mut() {
                        list.push(e);
                    }
                }
                self.field_proof_scope_cache
                    .borrow_mut()
                    .insert((b, block), bypass.clone());
                bypass
            }
        };
        let Some(bypass) = bypass else {
            return false;
        };
        if bypass.is_empty() {
            return true;
        }
        if !self.proven_preheader_on {
            return false;
        }
        let mut obligations = self.field_proof_osr_obligations.borrow_mut();
        for e in bypass {
            if !obligations.contains(&(e, NO_NODE, false)) {
                obligations.push((e, NO_NODE, false));
            }
        }
        true
    }

    /// True if any deopting layout-epoch guard dominates `block` (ignoring the
    /// guard's original receiver `NodeId`).
    pub(in crate::ir_lower) fn epoch_proof_holds(&self, block: usize) -> bool {
        if self.proven_scope_on {
            return self
                .layout_proven
                .iter()
                .any(|&(_, b)| b == block || self.epoch_proof_reaches(b, block));
        }
        let cross_block = self.layout_guard_cross_block
            && self.call_throw_entry_blocks.is_empty()
            && self.graph.catch_arm_sites.is_empty();
        self.layout_proven.iter().any(|&(_, b)| {
            b == block
                || (cross_block
                    && self.schedule.dom.is_reachable(b)
                    && self.schedule.dom.dominates(b, block))
        })
    }

    /// Hoist one VM-wide deopting `layout_replace_epoch_guard` to entry block 0
    /// when this method has a loop containing field accesses and block 0 can
    /// safely deopt at bci 0. OSR entries that bypass block 0 automatically
    /// receive an `OsrCheckInsn::LayoutEpoch` obligation via
    /// [`Self::epoch_proof_reaches`].
    pub(in crate::ir_lower) fn emit_entry_layout_epoch_guard_if_needed(&mut self) {
        if self.leaf_entry.plan.leaf_frame
            || !self.layout_guard_deopt_on
            || !self.proven_scope_on
            || !self.proven_preheader_on
        {
            return;
        }
        let (addr, _) = cratonvm_types::layout_replace_epoch_guard();
        if addr.is_null() || self.graph.method_self_lock.is_some() {
            return;
        }
        if self.schedule.blocks.is_empty() {
            return;
        }
        let cycles = self.field_proof_cycle_blocks();
        let is_epoch_user = |op: &Op| {
            matches!(
                op,
                Op::Load(_)
                    | Op::Store(_)
                    | Op::Unbox { .. }
                    | Op::ExactClassIs { .. }
                    | Op::CheckCast { .. }
                    | Op::InstanceOf { .. }
                    | Op::Call { .. }
            )
        };
        let has_loop_field = self.schedule.blocks.iter().enumerate().any(|(bi, blk)| {
            cycles.get(bi).copied().unwrap_or(false)
                && blk.nodes.iter().chain(blk.terminator.iter()).any(|&nid| {
                    self.graph
                        .nodes
                        .get(nid as usize)
                        .is_some_and(|n| is_epoch_user(&n.op))
                })
        });
        if !has_loop_field {
            return;
        }
        let entry_bci = self
            .schedule
            .blocks
            .first()
            .and_then(|b| self.graph.nodes.get(b.ctrl as usize))
            .and_then(|n| n.bytecode_pc)
            .unwrap_or(0);
        let can_emit_at_entry = entry_bci == 0
            && !self.pc_is_in_a_spliced_body(entry_bci)
            && self.safepoint_first_by_bci.contains_key(&entry_bci)
            && self.flat_site_frame_located(entry_bci)
            && self.call_throw_entry_blocks.is_empty()
            && (self.graph.catch_arm_sites.is_empty() || self.proven_catch_arms_on)
            && (0..self.schedule.blocks.len()).any(|bi| {
                cycles.get(bi).copied().unwrap_or(false)
                    && self.schedule.blocks[bi]
                        .nodes
                        .iter()
                        .chain(self.schedule.blocks[bi].terminator.iter())
                        .any(|&nid| {
                            self.graph
                                .nodes
                                .get(nid as usize)
                                .is_some_and(|n| is_epoch_user(&n.op))
                        })
                    && self.epoch_proof_reaches(0, bi)
            });
        if can_emit_at_entry && self.emit_layout_epoch_cmp() {
            self.emit_deopt_unless(0x84, entry_bci, DeoptReason::ClassLoading);
            self.layout_proven.push((NO_NODE, 0));
        }
        if !self.epoch_proof_holds(0) {
            self.plan_preheader_layout_epoch_guards();
        }
    }

    /// True on Gen/Serial (`GenHeap`): `JIT_READ_BOUNDS` stays live until heap `Drop`
    /// and slots hold plain pointers, so per-read `emit_read_bounds_live_check` is
    /// unnecessary on proven/trusted reference loads.
    pub(in crate::ir_lower) fn gen_bounds_always_live(&self) -> bool {
        self.ref_store_post_skip_mask.is_some()
            && self.zgc_fused_gate.is_none()
            && self.read_bounds_addr != 0
    }

    /// Branch to `slow` if `ZGC_READ_BARRIER_ARMED` is non-zero. Clobbers R11 & flags.
    pub(in crate::ir_lower) fn emit_zgc_unarmed_check(&mut self, slow: &mut Vec<usize>) {
        let addr = cratonvm_types::zgc_read_barrier_armed_addr();
        if let Ok(disp32) = i32::try_from(addr) {
            self.buf.emit(&[0x80, 0x3C, 0x25]); // CMP BYTE [disp32], 0
            self.buf.emit(&disp32.to_le_bytes());
            self.buf.emit_byte(0);
        } else {
            self.emit_mov_reg_imm64(R11, addr as u64);
            self.buf.emit(&[0x41, 0x80, 0x3B, 0x00]); // CMP BYTE [R11], 0
        }
        slow.push(self.emit_jcc_rel32(0x85)); // JNE -> slow
    }

    /// On plain-pointer ZGC (`zgc_fused_gate.is_some()` and no colored base),
    /// check that ZGC's read barrier is still unarmed and verify bit 63 of the
    /// loaded word (`TEST dst, dst; JS slow`) instead of consulting
    /// `JIT_READ_BOUNDS` (which ZGC decommits clear to 0).
    pub(in crate::ir_lower) fn emit_zgc_plain_ref_word_guard(
        &mut self,
        dst: u8,
        slow: &mut Vec<usize>,
    ) {
        debug_assert_ne!(dst, R11);
        self.emit_zgc_unarmed_check(slow);
        let rex = 0x48 | if dst >= 8 { 0x05 } else { 0x00 };
        let modrm = 0xC0 | ((dst & 7) << 3) | (dst & 7);
        self.buf.emit(&[rex, 0x85, modrm]); // TEST dst, dst
        slow.push(self.emit_jcc_rel32(0x88)); // JS -> slow (bit 63 colored tag)
    }

    /// True when ZGC plain-store reference barriers can run inline behind the
    /// fused/pre gate (`!narrow_oops_enabled()` and `zgc_fused_gate.is_some()`).
    pub(in crate::ir_lower) fn zgc_plain_inline_ref_store_ok(&self) -> bool {
        !cratonvm_types::narrow_oops_enabled() && self.zgc_fused_gate.is_some()
    }

    /// True when `value` is a `Op::InstanceOf` or `Cmp(Eq/Ne, InstanceOf, 0)` that
    /// proves `base != null` in `block`'s strictly dominated path.
    pub(in crate::ir_lower) fn dominating_instanceof_proves_non_null(
        &self,
        base: NodeId,
        block: usize,
    ) -> bool {
        let dom = &self.schedule.dom;
        if !dom.is_reachable(block) {
            return false;
        }
        let target_canon = self.field_proof_canon(base);
        let entries = self.field_proof_entry_blocks();
        let no_throw_entries =
            self.call_throw_entry_blocks.is_empty() && self.graph.catch_arm_sites.is_empty();
        for (tb, blk) in self.schedule.blocks.iter().enumerate() {
            if !dom.is_reachable(tb) || !dom.dominates(tb, block) {
                continue;
            }
            if !no_throw_entries {
                continue;
            }
            if entries.iter().any(|&e| {
                e != tb
                    && Self::osr_entry_runs_before_entering(self.schedule, e, Some(tb))
                        .get(block)
                        .copied()
                        .unwrap_or(true)
            }) {
                continue;
            }
            if blk.predecessors.len() != 1 {
                continue;
            }
            let Some(ctrl_node) = self.graph.nodes.get(blk.ctrl as usize) else {
                continue;
            };
            let proj_idx = match ctrl_node.op {
                Op::Proj(0) => 0u32,
                Op::Proj(1) => 1u32,
                _ => continue,
            };
            let Some(&if_id) = ctrl_node.inputs.first() else {
                continue;
            };
            let Some(if_node) = self.graph.nodes.get(if_id as usize) else {
                continue;
            };
            if if_node.op != Op::If || if_node.inputs.len() < 2 {
                continue;
            }
            let cond_id = if_node.inputs[1];
            let Some(cond) = self.graph.nodes.get(cond_id as usize) else {
                continue;
            };
            let inst_id = match (&cond.op, proj_idx) {
                (Op::InstanceOf { .. }, 0) => Some(cond_id),
                (Op::ExactClassIs { class_id }, 0) if *class_id != 0 => Some(cond_id),
                (Op::Cmp(CmpOp::Ne), 0) | (Op::Cmp(CmpOp::Eq), 1) if cond.inputs.len() == 2 => {
                    let is_zero = |x: NodeId| {
                        self.graph
                            .nodes
                            .get(x as usize)
                            .is_some_and(|n| matches!(n.op, Op::Const(0)))
                    };
                    if is_zero(cond.inputs[1]) {
                        Some(cond.inputs[0])
                    } else if is_zero(cond.inputs[0]) {
                        Some(cond.inputs[1])
                    } else {
                        None
                    }
                }
                _ => None,
            };
            let Some(inst_id) = inst_id else {
                continue;
            };
            let Some(inst) = self.graph.nodes.get(inst_id as usize) else {
                continue;
            };
            let proven_recv = match inst.op {
                Op::InstanceOf { .. } => inst.inputs.get(2).copied(),
                Op::ExactClassIs { class_id } if class_id != 0 => inst.inputs.first().copied(),
                _ => None,
            };
            if proven_recv.is_some_and(|v| v == base || self.field_proof_canon(v) == target_canon) {
                return true;
            }
        }
        false
    }

    /// Ultra-lean final-class `instanceof` miss arm using `typecheck_rows` status
    /// byte (`STATUS_HIERARCHY_ONLY`) directly, avoiding miss-memo stores and
    /// 4-hop display verdict traversal.
    pub(in crate::ir_lower) fn emit_instanceof_final_miss_fast(
        &mut self,
        site_name: Option<&str>,
        slow: &mut Vec<usize>,
        zero: &mut Vec<usize>,
    ) -> bool {
        let Some(rows) = self.typecheck_rows_base() else {
            return false;
        };
        self.emit_instanceof_final_miss_fast_with_rows(rows, site_name, slow, zero)
    }

    pub(in crate::ir_lower) fn emit_instanceof_final_miss_fast_with_rows(
        &mut self,
        rows: usize,
        site_name: Option<&str>,
        slow: &mut Vec<usize>,
        zero: &mut Vec<usize>,
    ) -> bool {
        if rows == 0 || rows % crate::typecheck_rows::ROW_BYTES != 0 {
            return false;
        }
        self.note_rcx_written();
        self.buf.emit(&[0x8B, 0x08]); // MOV ECX, [RAX]
        self.buf.emit(&[0x81, 0xF9]); // CMP ECX, ROWS_CAP
        self.buf
            .emit(&crate::typecheck_rows::ROWS_CAP.to_le_bytes());
        slow.push(self.emit_jcc_rel32(0x83)); // JAE slow (covers >= 0x8000_0000 proxy ids)
        self.buf
            .emit(&[0xC1, 0xE1, crate::typecheck_rows::ROW_SHIFT]); // SHL ECX, 6
        self.emit_mov_reg_imm64(R11, rows as u64);
        self.buf.emit(&[
            0x41,
            0x80,
            0x3C,
            0x0B,
            crate::typecheck_rows::STATUS_HIERARCHY_ONLY as u8,
        ]); // CMP BYTE [R11 + RCX], STATUS_HIERARCHY_ONLY
        slow.push(self.emit_jcc_rel32(0x85)); // JNE slow
        if !typecheck_site_is_boot_package(site_name) {
            let latch = cratonvm_types::duplicate_class_name_seen_addr();
            self.emit_ir_satb_pre_cmp(latch, true);
            slow.push(self.emit_jcc_rel32(0x85)); // JNE slow
        }
        zero.push(self.emit_jmp_rel32());
        true
    }

    /// Full `final_miss` inline `instanceof` arm: uses [`Self::emit_instanceof_final_miss_fast`]
    /// when type-check rows are available, falling back to the legacy verdict/memo path.
    pub(in crate::ir_lower) fn emit_instanceof_final_miss_arm(
        &mut self,
        cid: u32,
        site_name: Option<&str>,
        final_miss_verdict_cell: Option<usize>,
        slow: &mut Vec<usize>,
        zero: &mut Vec<usize>,
    ) {
        if self.emit_instanceof_final_miss_fast(site_name, slow, zero) {
            return;
        }
        self.buf.emit(&[0x83, 0x78, 0x00, 0x00]); // CMP DWORD [RAX+0], 0
        slow.push(self.emit_jcc_rel32(0x8C)); // JL -> helper (proxy ids)
        let latch = cratonvm_types::duplicate_class_name_seen_addr();
        self.emit_mov_reg_imm64(R11, latch as u64);
        self.buf.emit(&[0x41, 0x80, 0x3B, 0x00]); // CMP BYTE [R11], 0
        slow.push(self.emit_jcc_rel32(0x85)); // JNE -> helper
        let memo = final_miss_verdict_cell
            .filter(|_| crate::typecheck_miss_memo_enabled())
            .and_then(|_| crate::typecheck_miss_memo_cell(&self.direct_helpers, cid));
        if let Some(m) = memo {
            self.emit_typecheck_miss_memo_probe(m, zero);
        }
        if let Some(cell) = final_miss_verdict_cell {
            self.note_rcx_written();
            self.buf.emit(&[0x8B, 0x08]);
            let (bytes, edges) = typecheck_receiver_verdict_bytes(cell);
            let at = self.buf.pos();
            self.buf.emit(&bytes);
            slow.extend(edges.into_iter().map(|e| at + e));
        }
        if let Some(m) = memo {
            self.emit_typecheck_miss_memo_fill(m, cid, zero);
        }
        zero.push(self.emit_jmp_rel32());
    }

    /// Emit a fast whole-heap card mark using 64-bit SIB (`[R11 + R10]`) when
    /// baked hull + read-first apply, falling through when already dirty (or
    /// falling back to `emit_ir_whole_heap_card_mark` when unbaked/unconditional).
    /// Returns `true` if `emit_whole_heap_fallthrough` handled the mark (dirty
    /// falls through to the end of this function; clean/miss jump to `done`).
    pub(in crate::ir_lower) fn emit_ir_whole_heap_card_mark_sib_fast(
        &mut self,
        view_addr: usize,
        card_shift: u8,
        element: Option<u8>,
        to_helper: &mut Vec<usize>,
        done: &mut Vec<usize>,
    ) -> bool {
        let Some((lo, cards, map)) = crate::x64::card_view_hull_baked(view_addr) else {
            return false;
        };
        let summary = ir_card_view_has_summary(view_addr);
        let read_first = summary || crate::x64::jit_whole_heap_cond_card_mark_enabled();
        if !read_first || !crate::x64::jit_whole_heap_card_mark_sib_enabled() {
            return false;
        }
        let old_mask = self
            .ref_store_post_skip_mask
            .unwrap_or(cratonvm_types::GC_FLAG_OLD_GEN);
        let old_proven = crate::x64::jit_whole_heap_miss_young_skip_enabled()
            && old_mask == cratonvm_types::GC_FLAG_OLD_GEN;
        if !old_proven {
            // Null value creates no cross-generation edge (`RDX == value` on entry).
            self.buf.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
            done.push(self.emit_jcc_rel32(0x84)); // JZ -> done
        }
        // Skip young-generation receivers before card-hull arithmetic (`RAX == holder`).
        if crate::x64::jit_whole_heap_miss_young_skip_enabled() {
            self.buf.emit(&[
                0xF6,
                0x40,
                cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                old_mask,
            ]);
            done.push(self.emit_jcc_rel32(0x84)); // JZ -> done (young receiver)
        }
        match element {
            Some(d) => self.buf.emit(&[0x4C, 0x8D, 0x54, 0xC8, d]), // LEA R10, [RAX + RCX*8 + disp8]
            None => self.buf.emit(&[0x49, 0x89, 0xC2]),             // MOV R10, RAX
        }
        self.buf.emit(&[0x49, 0xC1, 0xEA, card_shift]); // SHR R10, imm8
        let miss = if old_proven {
            // Biased card base: the mark is inside the hull (see
            // `ir_biased_card_base`), so no hull compare and one constant.
            self.emit_mov_reg_imm64(R11, ir_biased_card_base(map, lo, card_shift) as u64);
            None
        } else {
            self.emit_mov_reg_imm64(R11, (lo >> card_shift) as u64);
            self.buf.emit(&[0x4D, 0x29, 0xDA]); // SUB R10, R11
            self.buf.emit(&[0x49, 0x81, 0xFA]); // CMP R10, imm32 (cards)
            self.buf.emit(&cards.to_le_bytes());
            let miss = self.emit_jcc_rel32(0x83); // JAE miss (outside hull)
            self.emit_mov_reg_imm64(R11, map as u64);
            Some(miss)
        };
        self.buf
            .emit(&[0x43, 0x80, 0x3C, 0x13, crate::x64::CARD_DIRTY_BYTE]); // CMP byte [R11 + R10], DIRTY
        done.push(self.emit_jcc_rel32(0x84)); // JE -> done (already dirty)
        if old_proven {
            // Deferred null check on the cold clean-card edge.
            self.buf.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
            done.push(self.emit_jcc_rel32(0x84)); // JZ -> done
        }

        // Cold clean edge.
        if summary {
            let summary_map = crate::x64::card_view_hull_summary_map(view_addr);
            // LEA R10, [R11 + R10] is not needed since we recompute summary+card below cleanly:
            for (extra, base) in [
                (IR_CARD_SUMMARY_SHIFT, summary_map),
                (0, map),
                (IR_CARD_SUMMARY_SHIFT, summary_map),
            ] {
                match element {
                    Some(d) => self.buf.emit(&[0x4C, 0x8D, 0x54, 0xC8, d]),
                    None => self.buf.emit(&[0x49, 0x89, 0xC2]),
                }
                self.buf.emit(&[0x49, 0xC1, 0xEA, card_shift]);
                self.emit_mov_reg_imm64(R11, (lo >> card_shift) as u64);
                self.buf.emit(&[0x4D, 0x29, 0xDA]); // SUB R10, R11
                if extra != 0 {
                    self.buf.emit(&[0x49, 0xC1, 0xEA, extra]);
                }
                self.emit_mov_reg_imm64(R11, base as u64);
                self.buf
                    .emit(&[0x43, 0xC6, 0x04, 0x13, crate::x64::CARD_DIRTY_BYTE]);
                // MOV byte [R11 + R10], DIRTY
            }
        } else {
            // R11 + R10 is the card byte under both bases (biased or exact).
            self.buf
                .emit(&[0x43, 0xC6, 0x04, 0x13, crate::x64::CARD_DIRTY_BYTE]); // MOV byte [R11 + R10], DIRTY
        }
        done.push(self.emit_jmp_rel32());

        if let Some(miss) = miss {
            self.patch_rel32_to_here(miss);
            self.emit_ir_card_post_clean(element, to_helper, done);
        }
        true
    }

    /// Precompute single-use `Op::ArrayLoad(MemKind::Byte)` -> `Op::And(raw, 0xFF)` pairs
    /// so `ArrayLoad(Byte)` emits `MOVZX` directly and `Op::And` forwards `raw`.
    pub(in crate::ir_lower) fn prepare_unsigned_byte_array_loads(&mut self) {
        let n = self.graph.nodes.len();
        self.unsigned_byte_array_loads = vec![false; n];
        self.elided_byte_mask_ands.clear();
        for (id, node) in self.graph.nodes.iter().enumerate() {
            if node.op != Op::And || node.ty != IrType::Int || node.inputs.len() != 2 {
                continue;
            }
            let id_nid = id as NodeId;
            let id_dropped = self.home_dropped.get(id).copied().unwrap_or(false);
            let id_dropped_ok = self.reg_alloc_on
                && self.publish_at_def_on
                && self.mir_mode == MirMode::Off
                && !self.ls_managed.get(id).copied().unwrap_or(false)
                && self
                    .assigned_gpr(id_nid)
                    .is_some_and(|r| r != RAX && r != RCX);
            if self.fused_test_and.get(id).copied().unwrap_or(false)
                || self.test_and_remat.get(id).copied().unwrap_or(false)
                || (id_dropped && !id_dropped_ok)
                || self.carry_names(id_nid)
            {
                continue;
            }
            let is_ff = |nid: NodeId| {
                self.graph
                    .nodes
                    .get(nid as usize)
                    .is_some_and(|kn| kn.ty == IrType::Int && matches!(kn.op, Op::Const(0xFF)))
            };
            let raw = if is_ff(node.inputs[1]) {
                node.inputs[0]
            } else if is_ff(node.inputs[0]) {
                node.inputs[1]
            } else {
                continue;
            };
            let raw_idx = raw as usize;
            let Some(rn) = self.graph.nodes.get(raw_idx) else {
                continue;
            };
            let raw_dropped = self.home_dropped.get(raw_idx).copied().unwrap_or(false);
            let raw_dropped_ok = self.reg_alloc_on && self.assigned_gpr(raw).is_some();
            if rn.op != Op::ArrayLoad(MemKind::Byte)
                || rn.ty != IrType::Int
                || self.use_count.get(raw_idx).copied().unwrap_or(0) != 1
                || self.deopt_named.get(raw_idx).copied().unwrap_or(true)
                || (raw_dropped && !raw_dropped_ok)
                || self.carry_names(raw)
            {
                continue;
            }
            self.unsigned_byte_array_loads[raw_idx] = true;
            self.elided_byte_mask_ands.insert(id_nid, raw);
        }
    }

    /// Lower an `Op::And(ArrayLoad(Byte), 0xFF)` whose mask was folded into
    /// `MOVZX` at `raw`, forwarding `raw` into `id`'s destination GPR or home.
    pub(in crate::ir_lower) fn emit_elided_byte_mask_and(
        &mut self,
        id: NodeId,
        raw: NodeId,
        ty: IrType,
    ) {
        let slot = self.alloc_slot(id);
        if let Some(dst) = self.integer_result_gpr(ty, slot) {
            if self.carry_names(raw) || self.resident_gpr(raw) != Some(dst) {
                self.gp_load_value(dst, raw);
            }
            self.publish_integer_result(dst, slot);
        } else {
            self.gp_load_value(RAX, raw);
            self.store_rax(slot);
        }
    }

    /// True when `id` is an `Int` `Op::And` with a non-negative `i32` constant
    /// mask and int high poisoning is off, so bits 31..63 are guaranteed zero.
    pub(in crate::ir_lower) fn index_known_nonneg_i32_masked(&self, id: NodeId) -> bool {
        if self.poison_int_high_on {
            return false;
        }
        let Some(node) = self.graph.nodes.get(id as usize) else {
            return false;
        };
        if node.op != Op::And || node.ty != IrType::Int || node.inputs.len() != 2 {
            return false;
        }
        let is_nonneg_mask = |nid: NodeId| {
            self.graph.nodes.get(nid as usize).is_some_and(
                |kn| matches!(kn.op, Op::Const(v) if v >= 0 && v <= i64::from(i32::MAX)),
            )
        };
        is_nonneg_mask(node.inputs[0]) || is_nonneg_mask(node.inputs[1])
    }

    /// RCX = a reference `aastore`'s index, zero-extended for `[array + RCX*8
    /// + HEADER]`. [`Self::gp_load_index_rcx`]'s 32-bit `MOV ECX, ...` already
    /// zero-extends (it marks `index_zx_at`), so the `MOV ECX, ECX` the gated
    /// and G1 stores used to append to a 64-bit load is emitted only for a
    /// carried index whose high half is unknown and is not a non-negative
    /// `i32` mask ([`Self::index_known_nonneg_i32_masked`]).
    pub(in crate::ir_lower) fn emit_ref_aastore_index_rcx(&mut self, index: NodeId) {
        self.gp_load_index_rcx(index);
        if self.index_zx_at != Some(self.buf.pos()) && !self.index_known_nonneg_i32_masked(index) {
            self.buf.emit(&[0x89, 0xC9]); // MOV ECX, ECX
        }
    }

    /// Fallback `jit_aastore` helper path for `Op::ArrayStore(MemKind::Ref)`,
    /// keeping `part_13.rs` under its file-size ratchet.
    pub(in crate::ir_lower) fn emit_ref_array_store_helper_fallback(
        &mut self,
        array: NodeId,
        index: NodeId,
        value: NodeId,
        inline_done: Vec<usize>,
        mut map: Option<SafepointMapToken>,
        map_on_helper_path: bool,
    ) {
        if map_on_helper_path {
            map = self.emit_safepoint_map_if_enabled();
        }
        // (vm_ptr, array_ptr, index, val), loaded in ARG order
        // so no load clobbers a later one's source.
        self.load_context(CALL_ARG_REGS[0]);
        self.gp_load_value(CALL_ARG_REGS[1], array);
        self.gp_load_value(CALL_ARG_REGS[2], index);
        if !self.index_known_nonneg_i32_masked(index) {
            let r = CALL_ARG_REGS[2];
            if r >= 8 {
                self.buf.emit_byte(0x45); // REX.R + REX.B
            }
            self.buf.emit(&[0x89, 0xC0 | ((r & 7) << 3) | (r & 7)]);
        }
        self.gp_load_value(CALL_ARG_REGS[3], value);
        self.emit_mov_reg_imm64(RAX, self.aastore as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
        self.note_inline_frame_return_site();
        if map_on_helper_path {
            if map.is_some() {
                self.emit_shadow_reload();
            }
            for done in inline_done {
                self.patch_rel32_to_here(done);
            }
        } else {
            for done in inline_done {
                self.patch_rel32_to_here(done);
            }
            if map.is_some() {
                self.emit_shadow_reload();
            }
        }
        note_ir_aastore_lowered();
    }

    /// Lean `Op::CheckCast` fast path for a loaded `final` non-array class:
    /// True when `value` is proven non-null AND was checked exact against
    /// `class_id` by an earlier `Op::CheckCast` in this block.
    pub(in crate::ir_lower) fn checkcast_exact_proven(&self, value: NodeId, class_id: u32) -> bool {
        let non_null = self.receiver_already_non_null(value)
            || self.block_guard_proves_non_null(value)
            || self
                .field_proof_block()
                .is_some_and(|b| self.dominating_instanceof_proves_non_null(value, b));
        if !non_null {
            return false;
        }
        let canon = self.field_proof_canon(value);
        self.checkcast_null_or_exact_proven
            .contains(&(value, class_id))
            || (canon != value
                && self
                    .checkcast_null_or_exact_proven
                    .contains(&(canon, class_id)))
    }

    /// Inline exact-class fast check for `Op::CheckCast` onto a `final` non-array class:
    /// null passes (`JZ hit`), arrays fall through to helper (`TEST KIND_TAGS`),
    /// and an exact class-word match joins `hit`, skipping displays and rows.
    /// Also records `(id, cid)` and `(obj, cid)` in `checkcast_null_or_exact_proven`
    /// so a dominated `Op::Unbox` in the same block skips its class-word compare.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn try_emit_exact_final_checkcast(
        &mut self,
        id: NodeId,
        obj: NodeId,
        slot: i32,
        name_ptr: usize,
        name_len: usize,
        cid: u32,
        _cast_proxy_cell: Option<usize>,
        result_forwarded: bool,
        hit_to_join: bool,
        hit_patch: &mut Option<usize>,
        to_join: &mut Vec<usize>,
    ) -> bool {
        if name_ptr == 0 || name_len == 0 {
            return false;
        }
        // SAFETY: `Op::CheckCast` names are interned `&'static str`s leaked for process lifetime.
        let first = unsafe { *(name_ptr as *const u8) };
        if first == b'['
            || (!crate::typecheck_target_is_final_class(name_ptr as *const u8)
                && !self.cha_leaf_classes.contains(&cid))
        {
            return false;
        }
        crate::CHECKCAST_INLINE_SITES_IR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.gp_load_value(RAX, obj);
        let non_null = self.check_elision.null_elided(id)
            || self.receiver_already_non_null(obj)
            || self.block_guard_proves_non_null(obj);
        let null_hit = if !non_null {
            self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
            Some(self.emit_jcc_rel32(0x84)) // JZ -> hit
        } else {
            None
        };
        self.buf.emit(&[
            0xF6,
            0x40,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
            cratonvm_types::KIND_TAG_BYTE_MASK,
        ]);
        let kind_slow = self.emit_jcc_rel32(0x85); // JNE -> helper
        self.buf.emit(&[0x81, 0x78, 0x00]); // CMP DWORD [RAX+0], cid
        self.buf.emit(&cid.to_le_bytes());
        let miss_slow = self.emit_jcc_rel32(0x85); // JNE -> helper
        let hit_at = self.buf.pos();
        if !result_forwarded {
            self.store_rax(slot);
        }
        *hit_patch = Some(self.emit_jmp_rel32());
        if let Some(nh) = null_hit {
            if hit_to_join {
                to_join.push(nh);
            } else {
                self.patch_rel32_to_target(nh, hit_at);
            }
        }
        self.patch_rel32_to_here(kind_slow);
        self.patch_rel32_to_here(miss_slow);
        if !self.checkcast_null_or_exact_proven.contains(&(id, cid)) {
            self.checkcast_null_or_exact_proven.push((id, cid));
        }
        if !self.checkcast_null_or_exact_proven.contains(&(obj, cid)) {
            self.checkcast_null_or_exact_proven.push((obj, cid));
        }
        if non_null {
            self.note_receiver_non_null(id);
            self.note_receiver_non_null(obj);
            if crate::ir::ir_unbox_guard_cse_enabled() {
                if !self.unbox_class_proven.contains(&(id, cid)) {
                    self.unbox_class_proven.push((id, cid));
                }
                if !self.unbox_class_proven.contains(&(obj, cid)) {
                    self.unbox_class_proven.push((obj, cid));
                }
            }
        }
        true
    }

    pub(in crate::ir_lower) fn plan_preheader_layout_epoch_guards(&mut self) {
        self.preheader_epoch_guards.clear();
        if !self.layout_guard_deopt_on
            || !self.proven_scope_on
            || !self.proven_preheader_on
            || !self.call_throw_entry_blocks.is_empty()
            || !self.graph.catch_arm_sites.is_empty()
            || self.graph.method_self_lock.is_some()
            || cratonvm_types::layout_replace_epoch_guard().0.is_null()
        {
            return;
        }
        let nb = self.schedule.blocks.len();
        if nb <= 1 {
            return;
        }
        // Identify blocks belonging to a CFG cycle (each block that can reach itself in >= 1 step).
        let mut in_cycle = vec![false; nb];
        let mut cycle_field_blocks: Vec<usize> = Vec::new();
        for cb in 0..nb {
            if !self.schedule.dom.is_reachable(cb) {
                continue;
            }
            let mut seen = vec![false; nb];
            let mut stack: Vec<usize> = self.schedule.blocks[cb].successors.clone();
            while let Some(b) = stack.pop() {
                if b == cb {
                    in_cycle[cb] = true;
                    break;
                }
                if b < nb && !seen[b] {
                    seen[b] = true;
                    stack.extend(self.schedule.blocks[b].successors.iter().copied());
                }
            }
            if in_cycle[cb] {
                let blk = &self.schedule.blocks[cb];
                let has_field_or_call = blk.nodes.iter().chain(blk.terminator.iter()).any(|&nid| {
                    matches!(
                        self.graph.nodes[nid as usize].op,
                        Op::Load(_)
                            | Op::Store(_)
                            | Op::Unbox { .. }
                            | Op::ExactClassIs { .. }
                            | Op::CheckCast { .. }
                            | Op::InstanceOf { .. }
                            | Op::Call { .. }
                    )
                });
                if has_field_or_call {
                    cycle_field_blocks.push(cb);
                }
            }
        }
        if cycle_field_blocks.is_empty() {
            return;
        }
        for pre_b in 0..nb {
            if in_cycle[pre_b] || !self.schedule.dom.is_reachable(pre_b) {
                continue;
            }
            if self
                .preheader_epoch_guards
                .keys()
                .any(|&g| self.schedule.dom.dominates(g, pre_b))
            {
                continue;
            }
            let reaches_loop_fields = cycle_field_blocks.iter().any(|&cb| {
                self.schedule.dom.dominates(pre_b, cb) && self.epoch_proof_reaches(pre_b, cb)
            });
            if !reaches_loop_fields {
                continue;
            }
            let blk = &self.schedule.blocks[pre_b];
            let candidate_bci = blk
                .nodes
                .iter()
                .chain(blk.terminator.iter())
                .filter_map(|&nid| self.graph.nodes.get(nid as usize)?.bytecode_pc)
                .find(|&bci| {
                    !self.pc_is_in_a_spliced_body(bci)
                        && !self.keep_set_chain_only_pcs.contains(&bci)
                        && self.preheader_flat_safepoint_ready(pre_b, bci)
                });
            if let Some(pre_bci) = candidate_bci {
                self.preheader_epoch_guards.insert(pre_b, pre_bci);
            }
        }
    }

    fn preheader_flat_safepoint_ready(&self, pre_b: usize, bci: usize) -> bool {
        let Some(&si) = self.safepoint_first_by_bci.get(&bci) else {
            return false;
        };
        let Some(sp) = self.graph.safepoints.get(si) else {
            return false;
        };
        sp.locals
            .iter()
            .chain(sp.stack.iter())
            .chain(sp.monitors.iter())
            .all(|&v| {
                if v == NO_NODE {
                    return true;
                }
                if !self.frame_value_located(v)
                    || self.site_named.get(v as usize).copied().unwrap_or(false)
                {
                    return false;
                }
                let Some(node) = self.graph.nodes.get(v as usize) else {
                    return false;
                };
                if matches!(
                    node.op,
                    Op::Const(_) | Op::ConstF(_) | Op::Param(_) | Op::Dead
                ) {
                    return true;
                }
                let Some(&b_v) = self.schedule.node_to_block.get(v as usize) else {
                    return false;
                };
                if b_v >= self.schedule.blocks.len() || !self.schedule.dom.is_reachable(b_v) {
                    return false;
                }
                if matches!(node.op, Op::Phi) {
                    self.schedule.dom.dominates(b_v, pre_b)
                } else {
                    b_v != pre_b && self.schedule.dom.dominates(b_v, pre_b)
                }
            })
    }

    pub(in crate::ir_lower) fn emit_preheader_layout_epoch_guard(&mut self, block_idx: usize) {
        let Some(&pre_bci) = self.preheader_epoch_guards.get(&block_idx) else {
            return;
        };
        if self.epoch_proof_holds(block_idx) {
            return;
        }
        let saved_frame = self.cur_node_frame.take();
        let ready = self.flat_site_frame_located(pre_bci);
        self.cur_node_frame = saved_frame;
        if !ready {
            return;
        }
        if self.emit_layout_epoch_cmp() {
            self.emit_deopt_unless(0x84, pre_bci, DeoptReason::ClassLoading);
            self.layout_proven.push((NO_NODE, block_idx));
        }
    }

    pub(in crate::ir_lower) fn field_site_no_preceding_side_effects(
        &self,
        pc: usize,
        own: Option<u32>,
    ) -> bool {
        let Some(cur_id) = self.cur_def else {
            return false;
        };
        let Some(cur_node) = self.graph.nodes.get(cur_id as usize) else {
            return false;
        };
        if !op_is_readonly_trapping(&cur_node.op) {
            return false;
        }
        let same_site = |n: &Node| {
            n.bytecode_pc == Some(pc)
                && n.frame_snapshot
                    .filter(|_| node_names_own_frame_at_its_pc(self.graph, n))
                    == own
        };
        let is_side_effect = |n: &Node| {
            n.op != Op::Dead
                && !op_cannot_deopt(&n.op)
                && !guard_const_true_cannot_trap(self.graph, n)
                && !op_is_readonly_trapping(&n.op)
        };
        let cur_b = self.cur_lower_block;
        for (b_idx, blk) in self.schedule.blocks.iter().enumerate() {
            if b_idx == cur_b {
                for &nid in &blk.nodes {
                    if nid == cur_id {
                        break;
                    }
                    if let Some(n) = self.graph.nodes.get(nid as usize) {
                        if same_site(n) && is_side_effect(n) {
                            return false;
                        }
                    }
                }
            } else {
                for &nid in blk.nodes.iter().chain(blk.terminator.iter()) {
                    if let Some(n) = self.graph.nodes.get(nid as usize) {
                        if same_site(n) && is_side_effect(n) {
                            return false;
                        }
                    }
                }
            }
        }
        true
    }

    pub(in crate::ir_lower) fn find_dead_add_remat_deopt_node(
        &self,
        after_nodes: &[NodeId],
        base_pos: usize,
        target_bci: usize,
        sp_indices: &[usize],
    ) -> Option<(usize, NodeId)> {
        let mut graph_count = 0usize;
        let mut single_nid = None;
        for (nid, node) in self.graph.nodes.iter().enumerate() {
            let nid = nid as NodeId;
            if node.bytecode_pc == Some(target_bci) && !self.node_cannot_deopt_here(node) {
                graph_count += 1;
                single_nid = Some(nid);
            }
        }
        if graph_count == 1 {
            let nid = single_nid?;
            let dpos = after_nodes.iter().position(|&n| n == nid)?;
            return Some((base_pos + dpos, nid));
        }
        if sp_indices.len() != 1 {
            return None;
        }
        let si = sp_indices[0];
        let snap = u32::try_from(si).ok()?;
        let is_first = self.safepoint_first_by_bci.get(&target_bci) == Some(&si);
        let mut matched: Option<(usize, NodeId)> = None;
        for (nid, node) in self.graph.nodes.iter().enumerate() {
            if self.node_cannot_deopt_here(node) {
                continue;
            }
            if is_first && node.bytecode_pc == Some(target_bci) && node.frame_snapshot.is_none() {
                return None;
            }
            if node.frame_snapshot == Some(snap) {
                if node.bytecode_pc != Some(target_bci) {
                    return None;
                }
                if matched.is_some() {
                    return None;
                }
                let nid = nid as NodeId;
                let dpos = after_nodes.iter().position(|&n| n == nid)?;
                matched = Some((base_pos + dpos, nid));
            }
        }
        matched
    }

    pub(in crate::ir_lower) fn array_known_non_boolean(&self, arr_id: NodeId) -> bool {
        let mut work = vec![arr_id];
        let mut visited = Vec::new();
        let mut saw_base = false;
        let byte_tag = cratonvm_types::primitive_array_kind_tags_byte("[B");
        while let Some(cur) = work.pop() {
            if cur == NO_NODE || visited.contains(&cur) {
                continue;
            }
            visited.push(cur);
            if visited.len() > 32 {
                return false;
            }
            if self.graph.byte_array_nodes.contains(&cur) {
                saw_base = true;
                continue;
            }
            if self
                .graph
                .const_ref_meta
                .get(&(cur as usize))
                .is_some_and(|m| m.prim_array_tag.is_some() && m.prim_array_tag == byte_tag)
            {
                saw_base = true;
                continue;
            }
            let Some(node) = self.graph.nodes.get(cur as usize) else {
                return false;
            };
            match &node.op {
                Op::NewArray {
                    element_type: 8, ..
                } => {
                    saw_base = true;
                }
                Op::CheckCast { .. } => {
                    let Some(&inner) = node.inputs.get(2) else {
                        return false;
                    };
                    work.push(inner);
                }
                Op::Phi => {
                    if node.inputs.len() <= 1 {
                        return false;
                    }
                    for &inp in &node.inputs[1..] {
                        work.push(inp);
                    }
                }
                Op::Load(MemKind::Ref) => {
                    if !self.ref_field_load_known_byte_array(cur, node) {
                        return false;
                    }
                    saw_base = true;
                }
                _ => return false,
            }
        }
        saw_base
    }

    fn ref_field_load_known_byte_array(&self, load_id: NodeId, node: &Node) -> bool {
        let Some(ref layout) = self.graph.string_layout else {
            return false;
        };
        let Some(&idx_id) = node.inputs.get(3) else {
            return false;
        };
        let Some(Op::Const(field_idx)) = self.graph.nodes.get(idx_id as usize).map(|n| &n.op)
        else {
            return false;
        };
        let Some(&recv) = node.inputs.get(2) else {
            return false;
        };
        let row = self.graph.field_rows.get(&load_id).copied();
        if layout.has_coder
            && *field_idx == layout.value_field_index as i64
            && row
                .is_some_and(|(off, is_ref, _)| is_ref && off as i32 == layout.value_compact_offset)
            && layout.string_class_id != 0
            && self
                .schedule
                .node_to_block
                .get(load_id as usize)
                .is_some_and(|&b| {
                    self.dominating_exact_class(recv, b) == Some(layout.string_class_id)
                })
        {
            return true;
        }
        if let Some(b) = layout.builder {
            if layout.has_coder
                && b.class_id != 0
                && *field_idx == i64::from(b.value_field_index)
                && row
                    .is_some_and(|(off, is_ref, _)| is_ref && off as i32 == b.value_compact_offset)
                && self
                    .schedule
                    .node_to_block
                    .get(load_id as usize)
                    .is_some_and(|&blk| self.dominating_exact_class(recv, blk) == Some(b.class_id))
            {
                return true;
            }
        }
        false
    }

    pub(in crate::ir_lower) fn final_instanceof_class_id(op: &Op) -> Option<u32> {
        let Op::InstanceOf {
            name_ptr, name_len, ..
        } = *op
        else {
            return None;
        };
        if name_ptr == 0 || name_len == 0 {
            return None;
        }
        let ptr = name_ptr as *const u8;
        if !crate::typecheck_target_is_final_class(ptr) {
            return None;
        }
        let cid = crate::typecheck_target_for_site(ptr).filter(|&id| id != 0)?;
        // SAFETY: `typecheck_target_is_final_class` succeeded, so `ptr` is an interned target name.
        let first = unsafe { *ptr };
        if first == b'[' {
            return None;
        }
        Some(cid)
    }

    pub(in crate::ir_lower) fn exact_class_from_guard_cond(
        &self,
        cond: &Node,
    ) -> Option<(NodeId, u32)> {
        match &cond.op {
            Op::ExactClassIs { class_id } if *class_id != 0 && !cond.inputs.is_empty() => {
                Some((cond.inputs[0], *class_id))
            }
            Op::InstanceOf { .. } if cond.inputs.len() >= 3 => {
                Self::final_instanceof_class_id(&cond.op).map(|cid| (cond.inputs[2], cid))
            }
            Op::Cmp(CmpOp::Ne) if cond.inputs.len() == 2 => {
                let is_zero = |x: NodeId| {
                    self.graph
                        .nodes
                        .get(x as usize)
                        .is_some_and(|n| matches!(n.op, Op::Const(0)))
                };
                let inner_id = if is_zero(cond.inputs[1]) {
                    cond.inputs[0]
                } else if is_zero(cond.inputs[0]) {
                    cond.inputs[1]
                } else {
                    return None;
                };
                let inner = self.graph.nodes.get(inner_id as usize)?;
                self.exact_class_from_guard_cond(inner)
            }
            _ => None,
        }
    }

    pub(in crate::ir_lower) fn exact_class_from_branch_cond(
        &self,
        cond: &Node,
        proj: u32,
    ) -> Option<(NodeId, u32)> {
        if proj == 0 {
            return self.exact_class_from_guard_cond(cond);
        }
        if proj == 1 {
            if let Op::Cmp(CmpOp::Eq) = cond.op {
                if cond.inputs.len() == 2 {
                    let is_zero = |x: NodeId| {
                        self.graph
                            .nodes
                            .get(x as usize)
                            .is_some_and(|n| matches!(n.op, Op::Const(0)))
                    };
                    let inner_id = if is_zero(cond.inputs[1]) {
                        cond.inputs[0]
                    } else if is_zero(cond.inputs[0]) {
                        cond.inputs[1]
                    } else {
                        return None;
                    };
                    let inner = self.graph.nodes.get(inner_id as usize)?;
                    return self.exact_class_from_guard_cond(inner);
                }
            }
        }
        None
    }

    pub(in crate::ir_lower) fn fuse_final_instanceof_sites(
        &self,
        use_count: &[u32],
        deopt_named: &[bool],
        fused_cmp: &mut [bool],
    ) {
        let nb = self.schedule.blocks.len();
        let is_zero = |x: NodeId| {
            self.graph
                .nodes
                .get(x as usize)
                .is_some_and(|n| matches!(n.op, Op::Const(0)))
        };
        let can_fuse_cond = |id: NodeId, blk: usize| {
            let i = id as usize;
            use_count.get(i).copied().unwrap_or(0) == 1
                && !deopt_named.get(i).copied().unwrap_or(true)
                && self.schedule.node_to_block.get(i).copied() == Some(blk)
        };
        for (b_idx, blk) in self.schedule.blocks.iter().enumerate() {
            if b_idx >= nb {
                continue;
            }
            if let Some(tid) = blk.terminator {
                if let Some(tn) = self.graph.nodes.get(tid as usize) {
                    if tn.op == Op::If && tn.inputs.len() >= 2 {
                        let cond = tn.inputs[1];
                        if let Some(cn) = self.graph.nodes.get(cond as usize) {
                            if Self::final_instanceof_class_id(&cn.op).is_some()
                                && cn.inputs.len() >= 3
                                && !fused_cmp.get(cond as usize).copied().unwrap_or(true)
                                && can_fuse_cond(cond, b_idx)
                            {
                                fused_cmp[cond as usize] = true;
                            } else if matches!(cn.op, Op::Cmp(CmpOp::Eq | CmpOp::Ne))
                                && cn.inputs.len() == 2
                                && can_fuse_cond(cond, b_idx)
                            {
                                let inst = if is_zero(cn.inputs[1]) {
                                    Some(cn.inputs[0])
                                } else if is_zero(cn.inputs[0]) {
                                    Some(cn.inputs[1])
                                } else {
                                    None
                                };
                                if let Some(inst_id) = inst {
                                    if let Some(in_node) = self.graph.nodes.get(inst_id as usize) {
                                        if Self::final_instanceof_class_id(&in_node.op).is_some()
                                            && in_node.inputs.len() >= 3
                                            && !fused_cmp
                                                .get(inst_id as usize)
                                                .copied()
                                                .unwrap_or(true)
                                            && can_fuse_cond(inst_id, b_idx)
                                        {
                                            fused_cmp[cond as usize] = true;
                                            fused_cmp[inst_id as usize] = true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            for &nid in &blk.nodes {
                let Some(gn) = self.graph.nodes.get(nid as usize) else {
                    continue;
                };
                if !matches!(gn.op, Op::Guard { .. })
                    || gn.inputs.len() < 2
                    || self.implicit_throw_plans.contains_key(&nid)
                    || self.check_elision.guard_elided(nid)
                {
                    continue;
                }
                let cond = gn.inputs[1];
                let Some(cn) = self.graph.nodes.get(cond as usize) else {
                    continue;
                };
                if Self::final_instanceof_class_id(&cn.op).is_some()
                    && cn.inputs.len() >= 3
                    && !fused_cmp.get(cond as usize).copied().unwrap_or(true)
                    && can_fuse_cond(cond, b_idx)
                {
                    fused_cmp[cond as usize] = true;
                } else if matches!(cn.op, Op::Cmp(CmpOp::Ne))
                    && cn.inputs.len() == 2
                    && can_fuse_cond(cond, b_idx)
                {
                    let inst = if is_zero(cn.inputs[1]) {
                        Some(cn.inputs[0])
                    } else if is_zero(cn.inputs[0]) {
                        Some(cn.inputs[1])
                    } else {
                        None
                    };
                    if let Some(inst_id) = inst {
                        if let Some(in_node) = self.graph.nodes.get(inst_id as usize) {
                            if Self::final_instanceof_class_id(&in_node.op).is_some()
                                && in_node.inputs.len() >= 3
                                && !fused_cmp.get(inst_id as usize).copied().unwrap_or(true)
                                && can_fuse_cond(inst_id, b_idx)
                            {
                                fused_cmp[cond as usize] = true;
                                fused_cmp[inst_id as usize] = true;
                            }
                        }
                    }
                }
            }
        }
    }

    pub(in crate::ir_lower) fn fused_final_instanceof_cmp_info(
        &self,
        cmp_id: NodeId,
    ) -> Option<(NodeId, u32, bool)> {
        if !self
            .fused_cmp
            .get(cmp_id as usize)
            .copied()
            .unwrap_or(false)
        {
            return None;
        }
        let cn = self.graph.nodes.get(cmp_id as usize)?;
        let invert = match cn.op {
            Op::Cmp(CmpOp::Ne) => false,
            Op::Cmp(CmpOp::Eq) => true,
            _ => return None,
        };
        if cn.inputs.len() != 2 {
            return None;
        }
        let is_zero = |x: NodeId| {
            self.graph
                .nodes
                .get(x as usize)
                .is_some_and(|n| matches!(n.op, Op::Const(0)))
        };
        let inst_id = if is_zero(cn.inputs[1]) {
            cn.inputs[0]
        } else if is_zero(cn.inputs[0]) {
            cn.inputs[1]
        } else {
            return None;
        };
        if !self
            .fused_cmp
            .get(inst_id as usize)
            .copied()
            .unwrap_or(false)
        {
            return None;
        }
        let inst = self.graph.nodes.get(inst_id as usize)?;
        let cid = Self::final_instanceof_class_id(&inst.op)?;
        let recv = *inst.inputs.get(2)?;
        Some((recv, cid, invert))
    }

    pub(in crate::ir_lower) fn fused_guard_exact_class_info(
        &self,
        cond_id: NodeId,
    ) -> Option<(u32, NodeId)> {
        if !self
            .fused_cmp
            .get(cond_id as usize)
            .copied()
            .unwrap_or(false)
        {
            return None;
        }
        let cn = self.graph.nodes.get(cond_id as usize)?;
        match cn.op {
            Op::ExactClassIs { class_id } if cn.inputs.len() == 1 => Some((class_id, cn.inputs[0])),
            Op::InstanceOf { .. } if cn.inputs.len() >= 3 => {
                Self::final_instanceof_class_id(&cn.op).map(|cid| (cid, cn.inputs[2]))
            }
            Op::Cmp(CmpOp::Ne) => {
                let (recv, cid, invert) = self.fused_final_instanceof_cmp_info(cond_id)?;
                (!invert).then_some((cid, recv))
            }
            _ => None,
        }
    }

    pub(in crate::ir_lower) fn note_fused_exact_class_guard_proven(
        &mut self,
        recv: NodeId,
        cid: u32,
    ) {
        self.note_receiver_non_null(recv);
        if !self.checkcast_null_or_exact_proven.contains(&(recv, cid)) {
            self.checkcast_null_or_exact_proven.push((recv, cid));
        }
        if crate::ir::ir_unbox_guard_cse_enabled()
            && !self.unbox_class_proven.contains(&(recv, cid))
        {
            self.unbox_class_proven.push((recv, cid));
        }
    }

    /// Fuse single-use in-block `Op::And(x, mask)` into array index formation
    /// (`gp_load_index_rcx`), skipping the intermediate home store and reload.
    pub(in crate::ir_lower) fn plan_fused_index_ands(&mut self) {
        let n = self.graph.nodes.len();
        self.fused_index_and = vec![None; n];
        if self.mir_mode != MirMode::Off || !self.elide_unnamed_homes_on {
            return;
        }
        let color = |id: NodeId| {
            self.slot_plan
                .node_color
                .get(id as usize)
                .copied()
                .flatten()
        };
        let mut plan: Vec<(NodeId, NodeId, u32)> = Vec::new();
        for block in &self.schedule.blocks {
            for (cpos, &cons) in block.nodes.iter().enumerate() {
                let Some(cnode) = self.graph.nodes.get(cons as usize) else {
                    continue;
                };
                let bounds_safe = self.node_cannot_deopt_here(cnode)
                    || (self.check_elision.bounds_elided(cons)
                        && self.check_elision.null_elided(cons))
                    || self.mask_guard_proven(cnode);
                if !matches!(cnode.op, Op::ArrayLoad(_) | Op::ArrayStore(_))
                    || cnode.inputs.len() < 4
                    || !bounds_safe
                {
                    continue;
                }
                let idx = cnode.inputs[3];
                let Some(apos) = block.nodes[..cpos].iter().rposition(|&nid| nid == idx) else {
                    continue;
                };
                let Some(inode) = self.graph.nodes.get(idx as usize) else {
                    continue;
                };
                if inode.op != Op::And
                    || inode.ty != IrType::Int
                    || inode.inputs.len() != 2
                    || self.use_count.get(idx as usize).copied() != Some(1)
                    || self
                        .deopt_named_reachable
                        .get(idx as usize)
                        .copied()
                        .unwrap_or(true)
                    || self.assigned_gpr(idx).is_some()
                    || self.assigned_xmm(idx).is_some()
                    || self.ls_managed.get(idx as usize).copied().unwrap_or(false)
                    || self.carry_names(idx)
                    || self.carry_of.get(idx as usize).copied().flatten().is_some()
                    || self
                        .test_and_remat
                        .get(idx as usize)
                        .copied()
                        .unwrap_or(false)
                    || self
                        .fused_test_and
                        .get(idx as usize)
                        .copied()
                        .unwrap_or(false)
                    || self.elided_byte_mask_ands.contains_key(&idx)
                {
                    continue;
                }
                let imm_mask = |nid: NodeId| -> Option<u32> {
                    let kn = self.graph.nodes.get(nid as usize)?;
                    if kn.ty != IrType::Int {
                        return None;
                    }
                    let Op::Const(v) = kn.op else {
                        return None;
                    };
                    (0..=i64::from(i32::MAX)).contains(&v).then_some(v as u32)
                };
                let (x, mask) = match (imm_mask(inode.inputs[1]), imm_mask(inode.inputs[0])) {
                    (Some(m), _) => (inode.inputs[0], m),
                    (None, Some(m)) => (inode.inputs[1], m),
                    (None, None) => continue,
                };
                if x == NO_NODE
                    || self.carry_names(x)
                    || self.carry_of.get(x as usize).copied().flatten().is_some()
                    || self
                        .graph
                        .nodes
                        .get(x as usize)
                        .is_none_or(|xn| xn.ty != IrType::Int)
                    || (self.assigned_gpr(x).is_none()
                        && (self.home_dropped.get(x as usize).copied().unwrap_or(true)
                            || self.planned_slot_off(x).is_err()))
                {
                    continue;
                }
                let (x_color, x_gp) = (color(x), self.assigned_gpr(x));
                let clobbered = block.nodes[apos + 1..cpos].iter().any(|&later| {
                    let c = color(later);
                    let r = self.assigned_gpr(later);
                    (c.is_some() && c == x_color)
                        || (r.is_some() && r == x_gp)
                        || self.graph.nodes.get(later as usize).is_some_and(|ln| {
                            !op_cannot_deopt(&ln.op)
                                && !matches!(ln.op, Op::ArrayLoad(_) | Op::Guard { .. })
                        })
                });
                if clobbered {
                    continue;
                }
                plan.push((idx, x, mask));
            }
        }
        if !plan.is_empty() && self.home_dropped.len() < n {
            self.home_dropped.resize(n, false);
        }
        for (idx, x, mask) in plan {
            self.fused_index_and[idx as usize] = Some((x, mask));
            self.home_dropped[idx as usize] = true;
        }
    }

    pub(in crate::ir_lower) fn try_elide_proven_checkcast(
        &mut self,
        id: NodeId,
        obj: NodeId,
        slot: i32,
        result_forwarded: bool,
        cid: u32,
    ) -> bool {
        let cur_b = self.cur_lower_block;
        let proven_exact = self.dominating_exact_class(obj, cur_b) == Some(cid)
            || (self.receiver_already_non_null(obj) && self.checkcast_exact_proven(obj, cid));
        if !proven_exact {
            return false;
        }
        if !self.checkcast_null_or_exact_proven.contains(&(id, cid)) {
            self.checkcast_null_or_exact_proven.push((id, cid));
        }
        if !self.checkcast_null_or_exact_proven.contains(&(obj, cid)) {
            self.checkcast_null_or_exact_proven.push((obj, cid));
        }
        self.note_receiver_non_null(id);
        self.note_receiver_non_null(obj);
        if crate::ir::ir_unbox_guard_cse_enabled() {
            if !self.unbox_class_proven.contains(&(id, cid)) {
                self.unbox_class_proven.push((id, cid));
            }
            if !self.unbox_class_proven.contains(&(obj, cid)) {
                self.unbox_class_proven.push((obj, cid));
            }
        }
        if result_forwarded {
            if let Some(cell) = self.defined_nodes.get_mut(id as usize) {
                *cell = false;
            }
        } else {
            self.gp_load_value(RAX, obj);
            self.store_rax(slot);
        }
        true
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn try_emit_exact_final_checkcast_or_outline(
        &mut self,
        id: NodeId,
        obj: NodeId,
        slot: i32,
        name_ptr: usize,
        name_len: usize,
        cid: u32,
        cast_proxy_cell: Option<usize>,
        result_forwarded: bool,
        hit_to_join: bool,
        sp_live_hi: i32,
        hit_patch: &mut Option<usize>,
        to_join: &mut Vec<usize>,
    ) -> (bool, bool) {
        if name_ptr == 0 || name_len == 0 {
            return (false, false);
        }
        let obj_is_null_const = self
            .graph
            .nodes
            .get(obj as usize)
            .is_some_and(|n| matches!(n.op, Op::Const(0)));
        let obj_slot_res = if obj_is_null_const {
            Ok(0)
        } else {
            self.slot_of_checked(obj)
        };
        let outline_slow = !self.tail_sink.cur
            && self.reloc_emit_on
            && Self::ir_gc_point_maps_enabled()
            && self.mir_mode == MirMode::Off
            && self.dead_ref_clear_site.is_none()
            && self.deferred_checkcast_tails.len() < 1024
            && !self.call_throw_plans.contains_key(&id)
            && self
                .poll_fp_reloads
                .get(&(TYPECHECK_FP_RELOAD_KEY | id as usize))
                .is_none_or(|v| v.is_empty())
            && !self.lazy_ref_sites.contains_key(&id)
            && self.checkcast != 0
            && self.gp_reg_of.get(id as usize).copied().flatten().is_none()
            && !self.home_dropped.get(id as usize).copied().unwrap_or(false)
            && !self.carry_names_producer(id)
            && obj_slot_res.is_ok();
        if !outline_slow {
            let fast = self.try_emit_exact_final_checkcast(
                id,
                obj,
                slot,
                name_ptr,
                name_len,
                cid,
                cast_proxy_cell,
                result_forwarded,
                hit_to_join,
                hit_patch,
                to_join,
            );
            return (fast, false);
        }
        // SAFETY: `Op::CheckCast` names are interned `&'static str`s leaked for process lifetime.
        let first = unsafe { *(name_ptr as *const u8) };
        if first == b'['
            || (!crate::typecheck_target_is_final_class(name_ptr as *const u8)
                && !self.cha_leaf_classes.contains(&cid))
        {
            return (false, false);
        }
        crate::CHECKCAST_INLINE_SITES_IR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.gp_load_value(RAX, obj);
        let non_null = self.check_elision.null_elided(id)
            || self.receiver_already_non_null(obj)
            || self.block_guard_proves_non_null(obj);
        let null_hit = if !non_null {
            self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
            Some(self.emit_jcc_rel32(0x84)) // JZ -> hit
        } else {
            None
        };
        self.buf.emit(&[
            0xF6,
            0x40,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
            cratonvm_types::KIND_TAG_BYTE_MASK,
        ]);
        let kind_slow = self.emit_jcc_rel32(0x85);
        self.buf.emit(&[0x81, 0x78, 0x00]); // CMP DWORD [RAX+0], cid
        self.buf.emit(&cid.to_le_bytes());
        let miss_slow = self.emit_jcc_rel32(0x85);
        let hit_at = self.buf.pos();
        if let Some(nh) = null_hit {
            self.patch_rel32_to_target(nh, hit_at);
        }
        if !result_forwarded {
            self.store_rax(slot);
        }
        let resume = self.buf.pos();
        if !self.checkcast_null_or_exact_proven.contains(&(id, cid)) {
            self.checkcast_null_or_exact_proven.push((id, cid));
        }
        if !self.checkcast_null_or_exact_proven.contains(&(obj, cid)) {
            self.checkcast_null_or_exact_proven.push((obj, cid));
        }
        if non_null {
            self.note_receiver_non_null(id);
            self.note_receiver_non_null(obj);
            if crate::ir::ir_unbox_guard_cse_enabled() {
                if !self.unbox_class_proven.contains(&(id, cid)) {
                    self.unbox_class_proven.push((id, cid));
                }
                if !self.unbox_class_proven.contains(&(obj, cid)) {
                    self.unbox_class_proven.push((obj, cid));
                }
            }
        }
        let was_defined = std::mem::replace(&mut self.defined_nodes[id as usize], false);
        let saved_pending_shadow = std::mem::take(&mut self.pending_shadow);
        let map_start = self.buf.mark();
        self.defer_frame_block_ensure = true;
        let map = self.emit_safepoint_map(sp_live_hi);
        self.defer_frame_block_ensure = false;
        let map_end = self.buf.pos();
        let slow_map_bytes = self.buf.as_slice()[map_start.len..map_end].to_vec();
        self.buf.rewind_to_mark(map_start);
        self.defined_nodes[id as usize] = if result_forwarded { false } else { was_defined };
        let slow_pending_shadow = std::mem::replace(&mut self.pending_shadow, saved_pending_shadow);
        let ensure_frame_block = !self.frame_block.is_empty() && map.is_some();
        self.deferred_checkcast_tails.push(DeferredCheckCastTail {
            slow: vec![kind_slow, miss_slow],
            slow_map_bytes,
            ensure_frame_block,
            map,
            pending_shadow: slow_pending_shadow,
            slot,
            result_forwarded,
            obj_slot: obj_slot_res.unwrap_or(0),
            name_ptr,
            name_len,
            helper: self.checkcast,
            resume,
            exit_bci: self.resume_bci(self.cur_bci),
            node_pc: self.cur_node_pc,
            site_bci: self.cur_bci,
            site_frame: self.cur_node_frame,
        });
        (true, true)
    }

    fn carry_names_producer(&self, id: NodeId) -> bool {
        self.carry_of.get(id as usize).copied().flatten().is_some()
            || self
                .tail_carry_of
                .get(id as usize)
                .copied()
                .flatten()
                .is_some()
    }

    pub(in crate::ir_lower) fn emit_deferred_checkcast_tails(&mut self) {
        let tails = std::mem::take(&mut self.deferred_checkcast_tails);
        if tails.is_empty() {
            return;
        }
        let saved_pc = self.cur_node_pc;
        let saved_bci = self.cur_bci;
        let saved_frame = self.cur_node_frame;
        let saved_pending_shadow = std::mem::take(&mut self.pending_shadow);
        for mut tail in tails {
            for p in tail.slow {
                self.patch_rel32_to_here(p);
            }
            self.cur_node_pc = tail.node_pc;
            self.cur_bci = tail.site_bci;
            self.cur_node_frame = tail.site_frame;
            if !tail.slow_map_bytes.is_empty() {
                self.buf.emit(&tail.slow_map_bytes);
            }
            if tail.ensure_frame_block && !self.frame_block.is_empty() {
                self.emit_frame_block_ensure();
            }
            self.load_context(CALL_ARG_REGS[0]);
            if tail.obj_slot > 0 {
                self.load_reg_from_frame(CALL_ARG_REGS[1], tail.obj_slot);
            } else {
                self.emit_mov_reg_imm64(CALL_ARG_REGS[1], 0);
            }
            self.emit_mov_reg_imm64(CALL_ARG_REGS[2], tail.name_ptr as u64);
            self.emit_mov_reg_imm64(CALL_ARG_REGS[3], tail.name_len as u64);
            self.emit_mov_reg_imm64(RAX, tail.helper as u64);
            self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
            let ret_addr = self.buf.pos();
            self.anchor_safepoint_return(tail.map.as_mut(), ret_addr);
            self.note_inline_frame_return_site_at(ret_addr);
            self.emit_post_call_frame_record();
            self.pending_shadow = tail.pending_shadow;
            self.emit_shadow_reload();
            self.emit_call_return_sentinel_tail(IrType::Ref);
            if !tail.result_forwarded {
                let mut bytes = FrameAccess::new();
                enc_frame_store(RAX, tail.slot, &mut bytes);
                self.buf.emit(bytes.as_slice());
            }
            let jmp = self.emit_jmp_rel32();
            self.patch_rel32_to_target(jmp, tail.resume);
        }
        self.pending_shadow = saved_pending_shadow;
        self.cur_node_pc = saved_pc;
        self.cur_bci = saved_bci;
        self.cur_node_frame = saved_frame;
    }

    pub(in crate::ir_lower) fn const_has_no_stack_readers(&self, id: NodeId) -> bool {
        let Some(cnode) = self.graph.nodes.get(id as usize) else {
            return false;
        };
        let is_int_or_long = matches!(cnode.ty, IrType::Int | IrType::Long);
        let is_ref_null = cnode.ty == IrType::Ref && matches!(cnode.op, Op::Const(0));
        if !is_int_or_long && !is_ref_null {
            return false;
        }
        if is_int_or_long {
            return true;
        }
        // `IrType::Ref` `Const(0)` may still hit `slot_of` in `part_07.rs` ref store/aastore slow paths
        // or helper calls if used as a stored value or call arg; drop only when every user reads via
        // `gp_load_value` / `emit_value_phi_copies` / `frame_value_for` / `Cmp` / `If`.
        for (uid, u) in self.graph.nodes.iter().enumerate() {
            if u.op == Op::Dead || !u.inputs.contains(&id) {
                continue;
            }
            match &u.op {
                Op::Phi | Op::Cmp(_) | Op::If | Op::Return | Op::Guard { .. } => {}
                Op::Store(_) | Op::ArrayStore(_) | Op::Call { .. } => return false,
                Op::Load(_) => {
                    if u.inputs.get(2) == Some(&id) {
                        return false;
                    }
                }
                _ => {
                    let _ = uid;
                    return false;
                }
            }
        }
        true
    }

    pub(in crate::ir_lower) fn drop_guaranteed_unused_const_homes(&mut self) {
        if !self.phi_copy_regs_on || !self.const_imm_on || self.mir_mode != MirMode::Off {
            return;
        }
        let n = self.home_dropped.len().min(self.graph.nodes.len());
        for id in 0..n {
            let nid = id as NodeId;
            if self.home_dropped[id]
                || self.elide_excluded.get(id).copied().unwrap_or(false)
                || self.gp_reg_of.get(id).copied().flatten().is_some()
                || self.ls_managed.get(id).copied().unwrap_or(false)
            {
                continue;
            }
            let node = &self.graph.nodes[id];
            if !matches!(node.op, Op::Const(_)) {
                continue;
            }
            if !self.const_has_no_stack_readers(nid) {
                continue;
            }
            // Only drop on `keep_deopt_image` retry when every use is guaranteed immediate-capable
            // (never calls `slot_of`).
            let safe = self.graph.nodes.iter().all(|u| {
                u.op == Op::Dead
                    || !u.inputs.contains(&nid)
                    || matches!(
                        u.op,
                        Op::Add
                            | Op::Sub
                            | Op::Mul
                            | Op::And
                            | Op::Or
                            | Op::Xor
                            | Op::Shl
                            | Op::Shr
                            | Op::UShr
                            | Op::Cmp(_)
                            | Op::If
                            | Op::Phi
                            | Op::Return
                            | Op::ArrayLoad(_)
                            | Op::NewArray { .. }
                            | Op::Guard { .. }
                    )
                    || (matches!(u.op, Op::Load(_) | Op::Store(_)) && u.inputs.get(3) == Some(&nid))
            });
            if safe {
                self.home_dropped[id] = true;
                self.const_homes_dropped += 1;
            }
        }
    }

    pub(in crate::ir_lower) fn ref_node_never_runs_gc(&self, node_id: NodeId) -> bool {
        let Some(node) = self.graph.nodes.get(node_id as usize) else {
            return false;
        };
        if op_cannot_deopt(&node.op) || guard_const_true_cannot_trap(self.graph, node) {
            return true;
        }
        match node.op {
            Op::ArrayLength | Op::Guard { .. } | Op::Div | Op::Rem => true,
            Op::ArrayLoad(kind) => {
                !matches!(kind, MemKind::Ref)
                    || !crate::x64::zgc_read_barrier_blocks_inline_fields()
            }
            Op::ArrayStore(kind) => !matches!(kind, MemKind::Ref),
            _ => false,
        }
    }

    pub(in crate::ir_lower) fn is_call_free_gc_free_loop_header(&self, block_idx: usize) -> bool {
        let nb = self.schedule.blocks.len();
        if block_idx == 0
            || block_idx >= nb
            || !self.schedule.dom.is_reachable(block_idx)
            || self.call_throw_entry_blocks.contains(&block_idx)
            || self.field_proof_entry_blocks().contains(&block_idx)
        {
            return false;
        }
        let hdr = &self.schedule.blocks[block_idx];
        // Require the single non-back-edge predecessor to be `block_idx - 1` (falling straight in).
        let has_valid_entry = hdr.predecessors.iter().any(|&p| p == block_idx - 1)
            && hdr.predecessors.iter().all(|&p| {
                p == block_idx - 1 || (p >= block_idx && self.schedule.dom.dominates(block_idx, p))
            });
        if !has_valid_entry {
            return false;
        }
        // Every block in the natural loop dominated by `block_idx` that can reach a back-edge
        // must execute zero GC-triggering operations.
        let mut in_loop = vec![false; nb];
        let mut work = Vec::new();
        for &p in &hdr.predecessors {
            if p >= block_idx && p < nb && self.schedule.dom.dominates(block_idx, p) && !in_loop[p]
            {
                in_loop[p] = true;
                work.push(p);
            }
        }
        if work.is_empty() {
            return false;
        }
        in_loop[block_idx] = true;
        while let Some(b) = work.pop() {
            if b == block_idx {
                continue;
            }
            for &p in &self.schedule.blocks[b].predecessors {
                if p < nb && self.schedule.dom.dominates(block_idx, p) && !in_loop[p] {
                    in_loop[p] = true;
                    work.push(p);
                }
            }
        }
        for (b, &member) in in_loop.iter().enumerate() {
            if !member {
                continue;
            }
            let blk = &self.schedule.blocks[b];
            for &nid in &blk.nodes {
                if !self.ref_node_never_runs_gc(nid) {
                    return false;
                }
            }
            if let Some(tid) = blk.terminator {
                if let Some(tn) = self.graph.nodes.get(tid as usize) {
                    if !op_cannot_deopt(&tn.op) {
                        return false;
                    }
                }
            }
        }
        true
    }

    pub(in crate::ir_lower) fn extend_poll_invariant_ref_reloads(
        &self,
        pred: usize,
        slow_clears: &[i32],
        reloads: &mut Vec<(u8, i16)>,
    ) {
        if !self.ref_residency_on || !self.reloc_emit_on || !Self::ir_gc_point_maps_enabled() {
            return;
        }
        for (reg_idx, owner) in self.gp_reg_owner.iter().enumerate() {
            let Some(id) = *owner else {
                continue;
            };
            let reg = reg_idx as u8;
            if !IR_LOWER_LS_GPRS.contains(&reg) || reloads.iter().any(|&(r, _)| r == reg) {
                continue;
            }
            let Some(node) = self.graph.nodes.get(id as usize) else {
                continue;
            };
            if node.ty != IrType::Ref
                || !self
                    .defined_nodes
                    .get(id as usize)
                    .copied()
                    .unwrap_or(false)
                || !self.gp_reg_live.get(id as usize).copied().unwrap_or(false)
                || self.home_dropped.get(id as usize).copied().unwrap_or(false)
            {
                continue;
            }
            let Some(home) = self.home_offset_of(id) else {
                continue;
            };
            if slow_clears.contains(&home) {
                continue;
            }
            let _ = pred;
            if let Ok(home16) = i16::try_from(home) {
                reloads.push((reg, home16));
            }
        }
    }

    pub(in crate::ir_lower) fn plan_fused_i2l_adds(&mut self) {
        let n = self.graph.nodes.len();
        self.fused_i2l = vec![false; n];
        if self.mir_mode != MirMode::Off {
            return;
        }
        let mut is_carry_target = vec![false; n];
        for &c in &self.carry_of {
            if let Some((_, cons)) = c {
                if let Some(cell) = is_carry_target.get_mut(cons as usize) {
                    *cell = true;
                }
            }
        }
        for (b_idx, blk) in self.schedule.blocks.iter().enumerate() {
            for &nid in &blk.nodes {
                let Some(an) = self.graph.nodes.get(nid as usize) else {
                    continue;
                };
                if an.op != Op::Add
                    || an.ty != IrType::Long
                    || an.inputs.len() < 2
                    || self
                        .test_and_remat
                        .get(nid as usize)
                        .copied()
                        .unwrap_or(false)
                {
                    continue;
                }
                for &cand in &an.inputs[..2] {
                    let ci = cand as usize;
                    let Some(cn) = self.graph.nodes.get(ci) else {
                        continue;
                    };
                    if cn.op != Op::I2L || cn.inputs.is_empty() {
                        continue;
                    }
                    let x = cn.inputs[0];
                    let other = if cand == an.inputs[0] {
                        an.inputs[1]
                    } else {
                        an.inputs[0]
                    };
                    let carry_ok = match self.carry_of.get(ci).copied().flatten() {
                        None => true,
                        Some((_, cons)) if cons == nid => true,
                        Some(_) => false,
                    };
                    if self.use_count.get(ci).copied().unwrap_or(0) != 1
                        || self.deopt_named.get(ci).copied().unwrap_or(true)
                        || self.schedule.node_to_block.get(ci).copied() != Some(b_idx)
                        || self.gp_reg_of.get(ci).copied().flatten().is_some()
                        || self.ls_managed.get(ci).copied().unwrap_or(false)
                        || !carry_ok
                        || self.carry_deferred.get(ci).copied().unwrap_or(false)
                        || is_carry_target[ci]
                        || self.carry_names(x)
                        || self.carry_names(other)
                        || self.fused_i2l[ci]
                    {
                        continue;
                    }
                    if let Some(cell) = self.carry_of.get_mut(ci) {
                        *cell = None;
                    }
                    self.fused_i2l[ci] = true;
                    break;
                }
            }
        }
    }

    pub(in crate::ir_lower) fn try_emit_fused_add_i2l(&mut self, id: NodeId, slot: i32) -> bool {
        let node = &self.graph.nodes[id as usize];
        if node.op != Op::Add || node.ty != IrType::Long || node.inputs.len() < 2 {
            return false;
        }
        let (a, b) = (node.inputs[0], node.inputs[1]);
        let (acc, x) = if self.fused_i2l.get(b as usize).copied().unwrap_or(false) {
            (a, self.graph.nodes[b as usize].inputs[0])
        } else if self.fused_i2l.get(a as usize).copied().unwrap_or(false) {
            (b, self.graph.nodes[a as usize].inputs[0])
        } else {
            return false;
        };
        let emit_movsxd_reg = |lower: &mut Self, reg: u8, src: NodeId| {
            if let Some(rx) = lower.resident_gpr(src) {
                if lower.int_already_sign_extended(src) {
                    lower.emit_mov_reg_reg64(reg, rx);
                } else {
                    let rex = 0x48 | if reg >= 8 { 4 } else { 0 } | if rx >= 8 { 1 } else { 0 };
                    lower
                        .buf
                        .emit(&[rex, 0x63, 0xC0 | ((reg & 7) << 3) | (rx & 7)]);
                }
            } else if lower
                .graph
                .nodes
                .get(src as usize)
                .is_some_and(|n| matches!(n.op, Op::Const(_)))
            {
                lower.gp_load_value(reg, src);
                if !lower.int_already_sign_extended(src) {
                    let rex = if reg >= 8 { 0x4D } else { 0x48 };
                    lower
                        .buf
                        .emit(&[rex, 0x63, 0xC0 | ((reg & 7) << 3) | (reg & 7)]);
                }
            } else if !lower.carry_names(src) {
                if let Ok(off) = lower.slot_of_checked(src) {
                    lower.token_read_check(src);
                    if reg == RAX
                        && lower.publish_reload_elide_on
                        && lower.int_already_sign_extended(src)
                        && lower
                            .rax_home_mirror
                            .is_some_and(|(s, home, _, end)| s == RAX && home == off && end == lower.buf.pos())
                    {
                        lower.publish_reload_elides.push((lower.buf.pos(), src));
                        return;
                    }
                    let rex = if reg >= 8 { 0x4C } else { 0x48 };
                    lower.buf.emit(&[rex, 0x63]);
                    lower.emit_rbp_modrm_disp(reg, off);
                    return;
                }
                lower.gp_load_value(reg, src);
                if !lower.int_already_sign_extended(src) {
                    let rex = if reg >= 8 { 0x4D } else { 0x48 };
                    lower
                        .buf
                        .emit(&[rex, 0x63, 0xC0 | ((reg & 7) << 3) | (reg & 7)]);
                }
            } else {
                lower.gp_load_value(reg, src);
                if !lower.int_already_sign_extended(src) {
                    let rex = if reg >= 8 { 0x4D } else { 0x48 };
                    lower
                        .buf
                        .emit(&[rex, 0x63, 0xC0 | ((reg & 7) << 3) | (reg & 7)]);
                }
            }
        };
        if self.carry_names(acc) {
            self.gp_load_value(RAX, acc);
            emit_movsxd_reg(self, RCX, x);
            self.buf.emit(&[0x48, 0x01, 0xC8]); // ADD RAX, RCX
            self.store_rax(slot);
            return true;
        }
        let dst = self.assigned_gpr(id);
        let acc_reg = self.resident_gpr(acc);
        if let (Some(d), Some(r_acc)) = (dst, acc_reg) {
            if d == r_acc && !self.carry_names(x) && self.carry_at_store(slot).is_none() {
                emit_movsxd_reg(self, RAX, x);
                // ADD d, RAX (`0x01 /r`, reg=RAX(0), rm=d)
                let rex = if d >= 8 { 0x49 } else { 0x48 };
                self.buf.emit(&[rex, 0x01, 0xC0 | (d & 7)]);
                self.publish_integer_result(d, slot);
                return true;
            }
        }
        if let Some(r_acc) = acc_reg {
            emit_movsxd_reg(self, RAX, x);
            // ADD RAX, r_acc
            let rex = if r_acc >= 8 { 0x4C } else { 0x48 };
            self.buf.emit(&[rex, 0x01, 0xC0 | ((r_acc & 7) << 3)]);
            self.store_rax(slot);
            return true;
        }
        emit_movsxd_reg(self, RAX, x);
        if !self.emit_alu_acc_imm(acc, 0x05, true) && !self.emit_alu_acc_frame(acc, 0x05, true) {
            self.gp_load_value(RCX, acc);
            self.buf.emit(&[0x48, 0x01, 0xC8]); // ADD RAX, RCX
        }
        self.store_rax(slot);
        true
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::ir_lower) fn assign_free_callee_saved_loop_invariants(
    graph: &Graph,
    schedule: &Schedule,
    plan: &SlotPlan,
    live: &crate::regalloc::LiveModel,
    clobbers: &[(usize, Vec<crate::regalloc::PhysReg>)],
    split_gp_regs: &[u8],
    gp_reg_of: &mut [Option<u8>],
    reg_of: &[Option<u8>],
    split_managed: &[bool],
) {
    let mut taken: Vec<u8> = gp_reg_of.iter().flatten().copied().collect();
    taken.extend_from_slice(split_gp_regs);
    taken.sort_unstable();
    taken.dedup();
    let mut free: Vec<u8> = ir_gp_file()
        .iter()
        .copied()
        .filter(|r| !taken.contains(r))
        .collect();
    if free.is_empty() {
        return;
    }
    let n = graph.nodes.len();
    let nb = schedule.blocks.len();
    // Detect blocks that belong to loops containing calls/allocs (so we prioritize call-free loops for Refs).
    let mut block_in_call_loop = vec![false; nb];
    for cb in 0..nb {
        let blk = &schedule.blocks[cb];
        let has_call = blk.nodes.iter().chain(blk.terminator.iter()).any(|&nid| {
            graph.nodes.get(nid as usize).is_some_and(|nd| {
                matches!(
                    nd.op,
                    Op::Call { .. }
                        | Op::IndyBridge { .. }
                        | Op::New { .. }
                        | Op::NewArray { .. }
                        | Op::NewCp { .. }
                        | Op::MonitorEnter
                        | Op::MonitorExit
                        | Op::StoreStatic { .. }
                        | Op::Store(_)
                        | Op::ArrayStore(MemKind::Ref)
                )
            })
        });
        if has_call {
            block_in_call_loop[cb] = true;
        }
    }
    let mut cands: Vec<(u64, usize)> = Vec::new();
    for id in 0..n {
        if gp_reg_of.get(id).copied().flatten().is_some()
            || reg_of.get(id).copied().flatten().is_some()
            || split_managed.get(id).copied().unwrap_or(false)
            || plan.node_color.get(id).copied().flatten().is_none()
        {
            continue;
        }
        let Some(node) = graph.nodes.get(id) else {
            continue;
        };
        let ty_ok = matches!(node.ty, IrType::Int | IrType::Long)
            || (ir_ref_residency_enabled() && node.ty == IrType::Ref);
        if !ty_ok || matches!(node.op, Op::Const(_) | Op::ConstF(_) | Op::Phi) {
            continue;
        }
        let weight = live.weight.get(id).copied().unwrap_or(0);
        if weight < 2 {
            continue;
        }
        let def_blk = schedule.node_to_block.get(id).copied().unwrap_or(nb);
        if def_blk >= nb || live.loop_depth.get(def_blk).copied().unwrap_or(0) > 0 {
            continue;
        }
        // Check whether any reader sits inside a loop, and for Refs prefer call-free loops.
        let nid = id as NodeId;
        let mut in_loop_use = false;
        let mut uses_in_call_block = false;
        for (uid, un) in graph.nodes.iter().enumerate() {
            if un.op == Op::Dead || !un.inputs.contains(&nid) {
                continue;
            }
            let ub = schedule.node_to_block.get(uid).copied().unwrap_or(nb);
            if ub < nb && live.loop_depth.get(ub).copied().unwrap_or(0) > 0 {
                in_loop_use = true;
                if block_in_call_loop[ub] {
                    uses_in_call_block = true;
                }
            }
        }
        if !in_loop_use {
            continue;
        }
        if node.ty == IrType::Ref && uses_in_call_block {
            continue;
        }
        // Rank loop-invariant hoisted arrays/refs/ints with loop weight, breaking ties by id.
        cands.push((weight, id));
    }
    cands.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for (_, id) in cands {
        let Some(range) = plan.range.get(id).copied().flatten() else {
            continue;
        };
        let Some(pos) = free.iter().position(|r| {
            !clobbered_within(
                clobbers,
                range.lo,
                range.hi,
                crate::regalloc::PhysReg::gp(*r),
            )
        }) else {
            continue;
        };
        let reg = free.remove(pos);
        gp_reg_of[id] = Some(reg);
        if free.is_empty() {
            break;
        }
    }
}

impl<'a> Lowerer<'a> {
    /// Cold deopt remat for `Op::Add(x, Const(imm))` with `use_count == 0`
    /// named only at safepoints of one single trapping node `d` (`Guard` or `Load`):
    /// skip emitting `ADD/LEA + MOV` in the loop body and rebuild `[rbp - slot] = x + imm`
    /// inside `d`'s cold deopt pads.
    pub(in crate::ir_lower) fn plan_dead_add_pad_remats(
        &mut self,
        recipes: &std::collections::HashSet<NodeId>,
    ) {
        if self.forced_chain_deopts {
            return;
        }
        let flag = |v: &[bool], id: NodeId| v.get(id as usize).copied().unwrap_or(false);
        let color = |id: NodeId| {
            self.slot_plan
                .node_color
                .get(id as usize)
                .copied()
                .flatten()
        };
        let mut used_deopts: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
        self.extra_dead_add_remats.clear();
        let mut add_plan: Vec<(NodeId, NodeId, i32, bool)> = Vec::new();
        for block in &self.schedule.blocks {
            for (apos, &a) in block.nodes.iter().enumerate() {
                let Some(an) = self.graph.nodes.get(a as usize) else {
                    continue;
                };
                if an.op != Op::Add
                    || !matches!(an.ty, IrType::Int | IrType::Long)
                    || an.inputs.len() != 2
                    || self.use_count.get(a as usize).copied() != Some(0)
                    || !flag(&self.deopt_named, a)
                    || flag(&self.test_and_remat, a)
                    || flag(&self.fused_test_and, a)
                    || flag(&self.ls_managed, a)
                    || flag(&self.home_dropped, a)
                    || self.assigned_gpr(a).is_some()
                    || self.assigned_xmm(a).is_some()
                    || self.planned_slot_off(a).is_err()
                    || recipes.contains(&a)
                    || self.carry_names(a)
                    || self.carry_of.get(a as usize).copied().flatten().is_some()
                {
                    continue;
                }
                let (x, imm) = match (self.alu_imm32(an.inputs[1]), self.alu_imm32(an.inputs[0])) {
                    (Some(imm), _) => (an.inputs[0], imm),
                    (None, Some(imm)) => (an.inputs[1], imm),
                    (None, None) => continue,
                };
                if x == NO_NODE
                    || self.value_flow.is_absorbed(x)
                    || self.is_dropped_const(x)
                    || flag(&self.ls_managed, x)
                    || self.carry_names(x)
                    || self.carry_of.get(x as usize).copied().flatten().is_some()
                    || !self
                        .graph
                        .nodes
                        .get(x as usize)
                        .is_some_and(|xn| xn.ty == an.ty)
                {
                    continue;
                }
                let x_in_frame = !flag(&self.home_dropped, x) && self.planned_slot_off(x).is_ok();
                let x_in_saved_gpr = self
                    .assigned_gpr(x)
                    .is_some_and(|r| IR_LOWER_LS_GPRS.contains(&r));
                if !x_in_frame && !x_in_saved_gpr {
                    continue;
                }
                // Collect every safepoint naming `a` and map each to its trapping
                // node in this block after `a`. Single-bci sets preserve the
                // legacy `find_dead_add_remat_deopt_node` lookup; multi-bci
                // unrolled copies attach to every matching trapping node in block.
                let mut sp_entries: Vec<(usize, usize)> = Vec::new();
                for (si, sp) in self.graph.safepoints.iter().enumerate() {
                    let names_a = sp
                        .locals
                        .iter()
                        .chain(sp.stack.iter())
                        .chain(sp.monitors.iter())
                        .copied()
                        .chain(sp.sb_virtual_nodes())
                        .any(|v| v == a);
                    if names_a {
                        sp_entries.push((si, sp.bci));
                    }
                }
                if sp_entries.is_empty() {
                    continue;
                }
                let first_bci = sp_entries[0].1;
                let multi_bci = sp_entries.iter().any(|&(_, b)| b != first_bci);
                let targets: Vec<(usize, NodeId)> = if !multi_bci {
                    let sp_indices: Vec<usize> = sp_entries.iter().map(|&(si, _)| si).collect();
                    let Some(found) = self.find_dead_add_remat_deopt_node(
                        &block.nodes[apos + 1..],
                        apos + 1,
                        first_bci,
                        &sp_indices,
                    ) else {
                        continue;
                    };
                    vec![found]
                } else {
                    let mut list: Vec<(usize, NodeId)> = Vec::new();
                    let mut ok = true;
                    for &(si, bci) in &sp_entries {
                        let Some(found) = self.find_dead_add_remat_deopt_node(
                            &block.nodes[apos + 1..],
                            apos + 1,
                            bci,
                            &[si],
                        ) else {
                            ok = false;
                            break;
                        };
                        if !list.iter().any(|&(_, prev)| prev == found.1) {
                            list.push(found);
                        }
                    }
                    if !ok || list.is_empty() {
                        continue;
                    }
                    list
                };
                let all_targets_ok = targets.iter().all(|&(_, d)| {
                    let dn = &self.graph.nodes[d as usize];
                    let Some(tbci) = dn.bytecode_pc else {
                        return false;
                    };
                    match dn.op {
                        Op::Guard { .. } => {
                            !self.implicit_throw_plans.contains_key(&d)
                                && dn.inputs.get(1).is_none_or(|&c| {
                                    self.test_guard_cond
                                        .get(c as usize)
                                        .copied()
                                        .flatten()
                                        .is_none()
                                })
                        }
                        Op::Load(_) => {
                            !dn.is_volatile_access()
                                && !self.implicit_throw_plans.contains_key(&d)
                                && dn.inputs.get(2).is_some_and(|&base| {
                                    self.check_elision.null_elided(d)
                                        || self.receiver_already_non_null(base)
                                        || (self.graph.catch_arm_sites.is_empty()
                                            && !u32::try_from(tbci).is_ok_and(|tb| {
                                                self.graph
                                                    .call_throw_sites
                                                    .iter()
                                                    .any(|s| s.bci == tb)
                                            }))
                                })
                        }
                        _ => false,
                    }
                });
                if !all_targets_ok {
                    continue;
                }
                let Some(&max_dpos) = targets.iter().map(|(dp, _)| dp).max() else {
                    continue;
                };
                let (x_color, x_gp) = (color(x), self.assigned_gpr(x));
                let clobbered = block.nodes[apos + 1..=max_dpos].iter().any(|&later| {
                    let c = color(later);
                    let r = self.assigned_gpr(later);
                    (c.is_some() && c == x_color) || (r.is_some() && r == x_gp)
                });
                if clobbered {
                    continue;
                }
                for &(_, d) in &targets {
                    if !used_deopts.contains(&d) || multi_bci {
                        used_deopts.insert(d);
                        add_plan.push((a, d, imm, an.ty == IrType::Long));
                    }
                }
            }
        }
        for (a, d, imm, wide) in add_plan {
            if let Some(cell) = self.test_and_remat.get_mut(a as usize) {
                *cell = true;
            }
            if let Some(cell) = self.test_guard_cond.get_mut(d as usize) {
                if cell.is_none() {
                    *cell = Some((a, imm, wide));
                } else {
                    self.extra_dead_add_remats
                        .entry(d)
                        .or_default()
                        .push((a, imm, wide));
                }
            }
        }
    }

    /// Attach a [`PadRemat`] (`is_add: true`) to any cold deopt pads pushed
    /// while lowering `deopt_id`.
    pub(in crate::ir_lower) fn attach_add_pad_remats(
        &mut self,
        deopt_id: NodeId,
        pads_before: usize,
    ) {
        if pads_before >= self.deopt_pads.len() {
            return;
        }
        let primary = self
            .test_guard_cond
            .get(deopt_id as usize)
            .copied()
            .flatten();
        let extras = self
            .extra_dead_add_remats
            .get(&deopt_id)
            .cloned()
            .unwrap_or_default();
        if primary.is_none() && extras.is_empty() {
            return;
        }
        let mut built: Vec<PadRemat> = Vec::new();
        for (a, imm, wide) in primary.into_iter().chain(extras) {
            let Some(an) = self.graph.nodes.get(a as usize) else {
                continue;
            };
            if an.op != Op::Add || an.inputs.len() != 2 {
                continue;
            }
            let x = match (self.alu_imm32(an.inputs[1]), self.alu_imm32(an.inputs[0])) {
                (Some(v), _) if v == imm => an.inputs[0],
                (_, Some(v)) if v == imm => an.inputs[1],
                _ => continue,
            };
            let frame_ok = !self.is_dropped_const(x)
                && !self.home_dropped.get(x as usize).copied().unwrap_or(true);
            let x_loc = if let Some(r) = self
                .resident_gpr(x)
                .filter(|&r| IR_LOWER_LS_GPRS.contains(&r))
            {
                XLoc::Reg(r)
            } else if frame_ok {
                match self.slot_of_checked(x) {
                    Ok(off) => XLoc::Frame(off),
                    Err(_) => {
                        self.latch_bailout(Bailout::with_context(
                            BailoutReason::UnallocatedValue { node: x },
                            format!("n{a}: pad add-remat missing frame slot for n{x}"),
                        ));
                        return;
                    }
                }
            } else {
                self.latch_bailout(Bailout::with_context(
                    BailoutReason::UnallocatedValue { node: x },
                    format!("n{a}: pad add-remat missing resident/frame input n{x}"),
                ));
                return;
            };
            let slot = match self.slot_of_checked(a) {
                Ok(s) => s,
                Err(_) => {
                    self.latch_bailout(Bailout::with_context(
                        BailoutReason::UnallocatedValue { node: a },
                        format!("n{a}: pad add-remat missing home slot"),
                    ));
                    return;
                }
            };
            built.push(PadRemat {
                x_loc,
                imm,
                wide,
                slot,
                is_add: true,
            });
        }
        let mut iter = built.into_iter();
        if let Some(first) = iter.next() {
            let rest: Vec<PadRemat> = iter.collect();
            for &(patch, _) in &self.deopt_pads[pads_before..] {
                self.deopt_pad_remats.insert(patch, first);
                if !rest.is_empty() {
                    self.extra_pad_remats.insert(patch, rest.clone());
                }
            }
        }
    }

    /// JIT rx27 wave 6: rebuild a [`PadRemat`]'s home word, first thing in its
    /// deopt pad. Clobbers RAX and the flags only.
    pub(in crate::ir_lower) fn emit_pad_remat(&mut self, r: PadRemat) {
        match r.x_loc {
            XLoc::Reg(reg) => self.emit_mov_reg_reg64(RAX, reg),
            XLoc::Frame(off) => self.load_reg_from_frame(RAX, off),
            XLoc::Rax => {}
        }
        // ADD (0x05) or AND (0x25) RAX/EAX, imm32.
        let opcode = if r.is_add { 0x05 } else { 0x25 };
        if r.wide {
            self.buf.emit(&[0x48, opcode]);
        } else {
            self.buf.emit(&[opcode]);
        }
        self.buf.emit(&r.imm.to_le_bytes());
        let mut bytes = FrameAccess::new();
        enc_frame_store(RAX, r.slot, &mut bytes);
        self.buf.emit(bytes.as_slice());
    }
}
