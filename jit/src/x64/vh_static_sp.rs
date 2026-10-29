// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 2 (lane VHCOMP, `CRATONVM_JIT_VH_CONST_STATIC`): the single-pass
//! twin of the optimizing tier's static-field constant-`VarHandle` arm
//! (`ir_lower::vh_static_const`, whose module comment says what is served).
//!
//! This backend has no graph naming the `getstatic` that produced the handle
//! operand, so, as the instance arm (`Compiler::emit_sp_vh_const_prefix`), it
//! picks the nearest preceding reference `getstatic` the VM keeps a static
//! cell for and PROVES the choice at run time: the operand must equal that
//! `static final`'s current value (`sp_vh_const_handle_compare_bytes`). A
//! wrong guess merely declines.
//!
//! ```text
//!   R11 = handle ; RCX = the static's value ; CMP RCX, R11 ; JNE call
//!   R10 = expected ; RDX = value
//!   <vh_static_screen_bytes> <vh_static_access_bytes>
//!   JMP hit                          ; the result in RAX
//! ```

use super::op_invoke::sp_vh_const_handle_compare_bytes;
use super::*;
use crate::ir_lower::vh_static_const::{
    vh_const_static_enabled, vh_static_access_bytes, vh_static_cell_addr, vh_static_screen_bytes,
    vh_static_site_shape,
};

impl Compiler {
    /// The static arm; `ops` is `[handle, values...]`. Returns the hit edge's
    /// `JMP rel32` patch site, or `None` with nothing emitted.
    pub(super) fn emit_sp_vh_const_static_prefix(
        &mut self,
        pc: usize,
        code: &[u8],
        info: &crate::JitInvokeInfo,
        ret_type: u8,
        ops: &[StackSlot],
    ) -> Option<usize> {
        let shape = vh_static_site_shape(info.method_name, info.descriptor)?;
        if !vh_const_static_enabled()
            || ops.len() != 1 + shape.values.len()
            || ret_type != shape.ret
            || ops.iter().any(|s| matches!(s, StackSlot::Xmm(..)))
        {
            return None;
        }
        let (cell, static_class, static_field) = self.sp_vh_static_candidate(code, pc)?;
        // Everything resolved before the first byte.
        let base_cell = self
            .direct_helpers
            .resolve_static_base(static_class, static_field)?;
        let (compare, compare_edges) = sp_vh_const_handle_compare_bytes(base_cell, static_field)?;
        let (screen, screen_edges) = vh_static_screen_bytes(cell, shape.kind);
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] vh-const-static (sp) {} @pc={pc} {}{} kind={} \
                 static=({static_class},{static_field}) cell=0x{cell:x}",
                self.method_label,
                info.method_name,
                info.descriptor,
                char::from(shape.kind),
            );
        }
        let mut decline: Vec<usize> = Vec::new();
        // The handle proof (writes RCX), then the operands, which the screen
        // leaves alone.
        self.load_slot_to_reg(R11, ops[0]);
        let at = self.buf.pos();
        self.buf.emit(&compare);
        decline.extend(compare_edges.into_iter().map(|e| at + e));
        if shape.has_expected() {
            self.load_slot_to_reg(R10, ops[1]);
            self.load_slot_to_reg(RDX, ops[2]);
        } else if shape.values.len() == 1 {
            self.load_slot_to_reg(RDX, ops[1]);
        }
        let at = self.buf.pos();
        self.buf.emit(&screen);
        decline.extend(screen_edges.into_iter().map(|e| at + e));
        self.buf
            .emit(&vh_static_access_bytes(shape.op, shape.kind));
        let hit = self.emit_jmp_rel32_patch();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.slot_mirror = None;
        Some(hit)
    }

    /// The nearest reference `getstatic` before `pc` of a field the VM keeps
    /// a static cell for, as `(cell, class id, static field index)`. A
    /// heuristic: the caller proves it at run time.
    fn sp_vh_static_candidate(&self, code: &[u8], pc: usize) -> Option<(usize, u32, usize)> {
        let mut best: Option<(usize, usize, u32, usize)> = None;
        for &(spc, class_id, field_index, type_tag, _) in &self.static_field_info {
            if spc >= pc
                || type_tag != b'L'
                || code.get(spc) != Some(&0xb2)
                || best.is_some_and(|b| b.0 > spc)
            {
                continue;
            }
            if let Some(cell) = vh_static_cell_addr(&self.direct_helpers, class_id, field_index) {
                best = Some((spc, cell, class_id, field_index));
            }
        }
        best.map(|(_, cell, class_id, field_index)| (cell, class_id, field_index))
    }
}
