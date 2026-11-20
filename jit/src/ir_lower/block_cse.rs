// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-basic-block CSE helpers for layout-epoch checks, ZGC read-bounds /
//! barriered reference field loads, and `Op::Unbox` exact-class / compact proofs.
//!
//! State is strictly scoped to a single straight-line basic block during lowering
//! and conservatively invalidated on block changes, buffer rewinds, control joins,
//! calls/safepoints, memory writes, rejoining slow paths, and spill-slot reuse.

#[allow(unused_imports)]
use super::*;

const BLOCK_CSE_FIELD_CAP: usize = 8;

#[derive(Clone, Copy, Debug)]
struct FieldLoadEntry {
    node_id: NodeId,
    base_canon: NodeId,
    field_index: u32,
    node_ty: IrType,
    cell_off: i32,
    type_tag: u8,
    slot_color: Option<u32>,
    slot_valid: bool,
}

#[derive(Clone, Copy, Debug)]
pub(in crate::ir_lower) struct BlockCseState {
    block_idx: usize,
    synced_idx: usize,
    sealed_prev: bool,
    last_buf_pos: usize,
    layout_epoch_checked: bool,
    read_bounds_live_checked: bool,
    field_loads: [Option<FieldLoadEntry>; BLOCK_CSE_FIELD_CAP],
    field_next: usize,
}

impl Default for BlockCseState {
    fn default() -> Self {
        Self {
            block_idx: usize::MAX,
            synced_idx: 0,
            sealed_prev: false,
            last_buf_pos: 0,
            layout_epoch_checked: false,
            read_bounds_live_checked: false,
            field_loads: [None; BLOCK_CSE_FIELD_CAP],
            field_next: 0,
        }
    }
}

impl BlockCseState {
    fn invalidate_all(&mut self) {
        self.layout_epoch_checked = false;
        self.read_bounds_live_checked = false;
        self.field_loads = [None; BLOCK_CSE_FIELD_CAP];
        self.field_next = 0;
    }

    fn reset_for(&mut self, block_idx: usize, synced_idx: usize, buf_pos: usize) {
        self.block_idx = block_idx;
        self.synced_idx = synced_idx;
        self.sealed_prev = false;
        self.last_buf_pos = buf_pos;
        self.invalidate_all();
    }

    fn invalidate_slot_color(&mut self, color: u32) {
        for slot in &mut self.field_loads {
            if let Some(entry) = slot {
                if entry.slot_color == Some(color) {
                    entry.slot_valid = false;
                }
            }
        }
    }

    fn push_field_load(&mut self, entry: FieldLoadEntry) {
        // Avoid keeping duplicate entries for the same canonical load key.
        for slot in &mut self.field_loads {
            if let Some(existing) = slot {
                if existing.base_canon == entry.base_canon
                    && existing.field_index == entry.field_index
                    && existing.node_ty == entry.node_ty
                    && existing.cell_off == entry.cell_off
                    && existing.type_tag == entry.type_tag
                {
                    *slot = Some(entry);
                    return;
                }
            }
        }
        let idx = self.field_next % BLOCK_CSE_FIELD_CAP;
        self.field_loads[idx] = Some(entry);
        self.field_next = idx + 1;
    }
}

fn block_cse_op_preserves_state(op: &Op) -> bool {
    op_cannot_deopt(op)
        || matches!(
            op,
            Op::Guard { .. }
                | Op::ArrayLength
                | Op::ArrayLoad(
                    MemKind::Int
                        | MemKind::Long
                        | MemKind::Byte
                        | MemKind::Char
                        | MemKind::Short
                        | MemKind::Float
                        | MemKind::Double
                )
        )
        || matches!(op, Op::Unbox { op: uop, .. } if uop.is_load())
}

pub(in crate::ir_lower) fn node_exact_non_null_class(
    graph: &Graph,
    value: NodeId,
    depth: u8,
) -> Option<u32> {
    let node = graph.nodes.get(value as usize)?;
    match &node.op {
        Op::New { class_id, .. } => Some(*class_id),
        Op::IndyRootRef { slot_addr } => graph
            .const_ref_meta
            .get(slot_addr)
            .filter(|m| m.non_null)
            .and_then(|m| m.exact_class_id),
        Op::Phi if depth < 4 => {
            let mut exact: Option<u32> = None;
            let mut seen_any = false;
            for &inp in node.inputs.iter().skip(1) {
                if inp == value {
                    continue;
                }
                let cid = node_exact_non_null_class(graph, inp, depth + 1)?;
                seen_any = true;
                match exact {
                    None => exact = Some(cid),
                    Some(prev) if prev == cid => {}
                    Some(_) => return None,
                }
            }
            if seen_any {
                exact
            } else {
                None
            }
        }
        _ => None,
    }
}

impl<'a> Lowerer<'a> {
    fn block_cse_sync(&self, state: &mut BlockCseState) -> Option<(NodeId, usize)> {
        let cur_id = self.cur_def?;
        if self.mir_mode == MirMode::Verify {
            state.invalidate_all();
            return None;
        }
        let block = self.schedule.blocks.get(self.cur_lower_block)?;
        let cur_idx = block.nodes.iter().position(|&nid| nid == cur_id)?;
        let buf_pos = self.buf.pos();
        if state.block_idx != self.cur_lower_block
            || buf_pos < state.last_buf_pos
            || cur_idx < state.synced_idx
        {
            state.reset_for(self.cur_lower_block, cur_idx, buf_pos);
            return Some((cur_id, cur_idx));
        }
        if state.synced_idx < cur_idx {
            let start = if state.sealed_prev {
                state.synced_idx + 1
            } else {
                state.synced_idx
            };
            for &mid_id in &block.nodes[start..cur_idx] {
                let preserves = self
                    .graph
                    .nodes
                    .get(mid_id as usize)
                    .is_some_and(|n| block_cse_op_preserves_state(&n.op));
                if !preserves {
                    state.invalidate_all();
                    break;
                }
                if let Some(color) = self
                    .slot_plan
                    .node_color
                    .get(mid_id as usize)
                    .copied()
                    .flatten()
                {
                    state.invalidate_slot_color(color);
                }
            }
            state.sealed_prev = false;
            state.synced_idx = cur_idx;
        }
        state.last_buf_pos = buf_pos;
        Some((cur_id, cur_idx))
    }

    /// True when the current straight-line basic block already executed a
    /// non-rejoining layout-epoch check with no intervening safepoint or call.
    pub(in crate::ir_lower) fn block_cse_layout_epoch_checked(&self) -> bool {
        let mut state = self.block_cse.get();
        let ok = self.block_cse_sync(&mut state).is_some() && state.layout_epoch_checked;
        self.block_cse.set(state);
        ok
    }

    /// Record that the current node just passed a non-rejoining layout-epoch check.
    pub(in crate::ir_lower) fn block_cse_mark_layout_epoch_checked(&mut self) {
        let mut state = self.block_cse.get();
        if self.block_cse_sync(&mut state).is_some() {
            state.layout_epoch_checked = true;
            state.last_buf_pos = self.buf.pos();
        }
        self.block_cse.set(state);
    }

    /// True when the current basic block already passed a non-rejoining
    /// `JIT_READ_BOUNDS` liveness check with no intervening safepoint or call.
    pub(in crate::ir_lower) fn block_cse_read_bounds_live_checked(&self) -> bool {
        let mut state = self.block_cse.get();
        let ok = self.block_cse_sync(&mut state).is_some() && state.read_bounds_live_checked;
        self.block_cse.set(state);
        ok
    }

    /// Record that the current node passed a non-rejoining `JIT_READ_BOUNDS` guard.
    pub(in crate::ir_lower) fn block_cse_mark_read_bounds_live_checked(&mut self) {
        let mut state = self.block_cse.get();
        if self.block_cse_sync(&mut state).is_some() {
            state.read_bounds_live_checked = true;
            state.last_buf_pos = self.buf.pos();
        }
        self.block_cse.set(state);
    }

    /// Look up an already-loaded, ZGC-barriered field result in the same basic
    /// block when no intervening safepoint, call, store, or slot/register
    /// clobber could have invalidated it.
    pub(in crate::ir_lower) fn block_cse_find_field_load(
        &self,
        base: NodeId,
        field_index: u32,
        node_ty: IrType,
        cell_off: i32,
        type_tag: u8,
    ) -> Option<NodeId> {
        let base_canon = self.field_proof_canon(base);
        let mut state = self.block_cse.get();
        let found = (|| {
            let (cur_id, _) = self.block_cse_sync(&mut state)?;
            for slot in state.field_loads.iter().rev().flatten() {
                if slot.node_id == cur_id
                    || slot.base_canon != base_canon
                    || slot.field_index != field_index
                    || slot.node_ty != node_ty
                    || slot.cell_off != cell_off
                    || slot.type_tag != type_tag
                {
                    continue;
                }
                if self
                    .carry_of
                    .get(slot.node_id as usize)
                    .copied()
                    .flatten()
                    .is_some()
                    || self
                        .tail_carry_of
                        .get(slot.node_id as usize)
                        .copied()
                        .flatten()
                        .is_some()
                {
                    continue;
                }
                let gpr_live = self.resident_gpr(slot.node_id).is_some();
                let slot_live = slot.slot_valid
                    && !self
                        .home_dropped
                        .get(slot.node_id as usize)
                        .copied()
                        .unwrap_or(false)
                    && self
                        .defined_nodes
                        .get(slot.node_id as usize)
                        .copied()
                        .unwrap_or(false);
                if gpr_live || slot_live {
                    return Some(slot.node_id);
                }
            }
            None
        })();
        self.block_cse.set(state);
        found
    }

    /// Record a completed field load in the current basic block. When
    /// `had_slow_rejoin` is true, any older block-CSE proofs and entries are
    /// invalidated before inserting the freshly barriered current node.
    pub(in crate::ir_lower) fn block_cse_record_field_load(
        &mut self,
        base: NodeId,
        field_index: u32,
        node_ty: IrType,
        cell_off: i32,
        type_tag: u8,
        had_slow_rejoin: bool,
    ) {
        let base_canon = self.field_proof_canon(base);
        let mut state = self.block_cse.get();
        if let Some((cur_id, _)) = self.block_cse_sync(&mut state) {
            if had_slow_rejoin {
                state.invalidate_all();
            }
            let slot_color = self
                .slot_plan
                .node_color
                .get(cur_id as usize)
                .copied()
                .flatten();
            if let Some(color) = slot_color {
                state.invalidate_slot_color(color);
            }
            let slot_valid = !self
                .home_dropped
                .get(cur_id as usize)
                .copied()
                .unwrap_or(false);
            state.push_field_load(FieldLoadEntry {
                node_id: cur_id,
                base_canon,
                field_index,
                node_ty,
                cell_off,
                type_tag,
                slot_color,
                slot_valid,
            });
            state.sealed_prev = true;
            state.last_buf_pos = self.buf.pos();
        }
        self.block_cse.set(state);
    }

    /// True when `recv` is provably non-null with exact class `class_id` at
    /// this `Op::Unbox` site (same-block `unbox_class_proven`, dominating
    /// `ExactClassIs`, `Op::New { class_id }`, or uniform `Phi` thereof).
    pub(in crate::ir_lower) fn unbox_exact_class_proven(
        &self,
        recv: NodeId,
        class_id: u32,
    ) -> bool {
        if crate::ir::ir_unbox_guard_cse_enabled()
            && self.unbox_class_proven.contains(&(recv, class_id))
        {
            return true;
        }
        if self
            .field_proof_block()
            .is_some_and(|b| self.dominating_exact_class(recv, b) == Some(class_id))
        {
            return true;
        }
        let canon = self.field_proof_canon(recv);
        node_exact_non_null_class(self.graph, recv, 0) == Some(class_id)
            || (canon != recv && node_exact_non_null_class(self.graph, canon, 0) == Some(class_id))
    }

    /// True when `layout_replace_epoch_guard` was already checked against the
    /// compile-baked epoch along all paths to this `Op::Unbox` (cross-block
    /// `epoch_proof_holds` or same-block `block_cse_layout_epoch_checked`).
    pub(in crate::ir_lower) fn unbox_layout_epoch_proven(&self) -> bool {
        (self.layout_guard_deopt_on
            && self
                .field_proof_block()
                .is_some_and(|b| self.epoch_proof_holds(b)))
            || self.block_cse_layout_epoch_checked()
    }

    /// True when `recv` in `Op::Unbox` is provably compact and its layout epoch
    /// has already been checked.
    pub(in crate::ir_lower) fn unbox_compact_proven(&self, recv: NodeId, class_id: u32) -> bool {
        if !self.unbox_layout_epoch_proven() {
            return false;
        }
        let fresh_compact = matches!(
            self.graph.nodes.get(recv as usize).map(|n| &n.op),
            Some(&Op::New { class_id: rcid, num_fields })
                if rcid == class_id
                    && cratonvm_types::compact_tlab_total_size(rcid, num_fields).is_some()
        );
        fresh_compact
            || self
                .field_proof_block()
                .is_some_and(|b| self.field_proof_holds(&self.compact_proven, recv, b, true))
    }

    /// Emit the delta staging, compact-vs-legacy dispatch (elided when
    /// `compact_proven`), and payload read/`LOCK XADD` for `Op::Unbox`.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn emit_unbox_delta_and_field_access(
        &mut self,
        node: &Node,
        uop: crate::ir::UnboxOp,
        recv: NodeId,
        class_id: u32,
        compact_off: i32,
        legacy_off: i32,
        bci: usize,
    ) {
        let compact_proven = self.unbox_compact_proven(recv, class_id);
        if !compact_proven {
            // TEST BYTE [RAX + GC_FLAGS_BYTE_OFFSET], GC_FLAG_COMPACT
            self.buf.emit(&[0xF6, 0x80]);
            self.buf
                .emit(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());
            self.buf.emit(&[cratonvm_types::GC_FLAG_COMPACT]);
        }
        // RDX = delta, RCX = a copy of it, for the read-modify-write families
        // only. Loaded AFTER all deopting guards so neither deopt edge has a
        // live RCX to preserve (Windows `DEOPT_ARG0 == RCX`).
        let wide = uop.is_wide();
        if !uop.is_load() {
            match uop.delta_imm() {
                Some(imm) => {
                    if wide {
                        // MOV RDX, imm32 (sign-extended: +1 or -1).
                        self.buf.emit(&[0x48, 0xC7, 0xC2]);
                        self.buf.emit(&imm.to_le_bytes());
                    } else {
                        self.buf.emit(&[0xBA]); // MOV EDX, imm32
                        self.buf.emit(&imm.to_le_bytes());
                    }
                }
                None => {
                    self.gp_load_value(RDX, node.inputs[3]);
                }
            }
            if wide {
                self.buf.emit(&[0x48, 0x89, 0xD1]); // MOV RCX, RDX
            } else {
                self.buf.emit(&[0x89, 0xD1]); // MOV ECX, EDX
            }
        }

        let emit_access = |lower: &mut Self, disp: i32| {
            match (uop.is_load(), wide) {
                (true, true) => lower.buf.emit(&[0x48, 0x8B, 0x80]), // MOV RAX, [RAX + disp32]
                (true, false) => lower.buf.emit(&[0x8B, 0x80]),      // MOV EAX, [RAX + disp32]
                // LOCK XADD [RAX + disp32], RCX/ECX
                (false, true) => lower.buf.emit(&[0xF0, 0x48, 0x0F, 0xC1, 0x88]),
                (false, false) => lower.buf.emit(&[0xF0, 0x0F, 0xC1, 0x88]),
            }
            lower.buf.emit(&disp.to_le_bytes());
        };

        if compact_proven {
            if uop.is_ffm_element_read() {
                self.emit_ffm_element_read_access(uop, compact_off, false, bci);
            } else {
                emit_access(self, compact_off);
            }
            return;
        }

        let to_legacy = self.emit_jcc_rel32(0x84); // JZ -> legacy
        if uop.is_ffm_element_read() {
            self.emit_ffm_element_read_access(uop, compact_off, false, bci);
        } else {
            emit_access(self, compact_off);
        }
        let done = self.emit_jmp_rel32();

        self.patch_rel32_to_here(to_legacy);
        if uop.is_ffm_element_read() {
            self.emit_ffm_element_read_access(uop, legacy_off, true, bci);
        } else {
            emit_access(self, legacy_off);
        }
        self.patch_rel32_to_here(done);
    }
}
