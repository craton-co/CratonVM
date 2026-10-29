// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 4 (lane VHCARD, `CRATONVM_JIT_SP_VH_CONST_GEN_CARD_EDGE`): the
//! single-pass twin of the optimizing tier's generational card edge
//! (`ir_lower::vh_gen_card_edge`). A gated constant-`VarHandle` reference
//! write whose gate screen says a card may be owed (an old holder, a non-null
//! value, old objects exist) declined to the `VarHandle` call here; it now
//! pays this backend's own PRE / POST card barrier, the pair every
//! single-pass reference store emits (`Compiler::emit_gen_card_barrier`,
//! whose doc carries the take-over argument), around the access:
//!
//! ```text
//! edge:                                  ; RAX = holder, R10 = expected, RDX = value
//!   <emit_gen_card_barrier(RAX, RDX, RAX)>  PRE: inline check, the call on its slow edge
//!   R10 = expected
//!   <vh_const_access_bytes>               ; RAX = the result
//!   [SUB RSP, 48 ; MOV [RSP+32], RAX]     ; keep the result (as the G1 post edge)
//!   RAX = holder ; RDX = value
//!   <emit_gen_card_barrier(RAX, RDX, RAX)>  POST
//!   [MOV RAX, [RSP+32] ; ADD RSP, 48]
//! ```
//!
//! The barrier's slow edge reloads the holder and the value from their slots
//! after its call, so the operands must live in a frame slot or a callee-saved
//! register (never a constant or the R8 / R9 cache); any other site keeps the
//! decline. The barrier's call never parks for a collection, so the result
//! held on the machine stack across it is safe (the G1 post edge's argument).

use super::op_invoke::{SP_VH_G1_KEEP_RESULT, SP_VH_G1_RESTORE_RESULT};
use super::*;
use crate::ir_lower::{vh_const_access_bytes, VhConstCore, VhConstOp};

/// `CRATONVM_JIT_SP_VH_CONST_GEN_CARD_EDGE` (j27 wave 4, lane VHCARD; perf,
/// **default ON**, `=0` keeps the single-pass decline). Also off under the
/// optimizing tier's `CRATONVM_JIT_VH_CONST_GEN_CARD_EDGE=0`. Read per site.
pub(super) fn sp_vh_const_gen_card_edge_enabled() -> bool {
    crate::ir_lower::vh_gen_card_edge::vh_const_gen_card_edge_enabled()
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_SP_VH_CONST_GEN_CARD_EDGE")
}

/// May the barrier's slow edge reload `slot` after its call?
fn survives_a_call(slot: StackSlot) -> bool {
    matches!(slot, StackSlot::Frame(_) | StackSlot::CalleeSaved(_))
}

impl Compiler {
    /// May `core` take the card edge? A gated (not G1) reference write, this
    /// compile has the inline card check and its barrier, every operand
    /// (`ops[1..]`: receiver, values) survives a call, and the switch is on.
    pub(super) fn sp_vh_gen_card_edge_admitted(&self, core: &VhConstCore, ops: &[StackSlot]) -> bool {
        core.gates.is_some()
            && core.g1.is_none()
            && core.kind == b'L'
            && core.op != VhConstOp::Get
            && ops.len() >= 3
            && ops[1..].iter().all(|&s| survives_a_call(s))
            && self.inline_card_mark_available()
            && self.heap_local_offset != 0 // the barrier call's VM context slot
            && sp_vh_const_gen_card_edge_enabled()
    }

    /// The edge of the module comment, entered with RAX / R10 / RDX the
    /// operands. Returns the access's own decline fields (its cast screens,
    /// which run before any write); the caller patches them to the call.
    pub(super) fn emit_sp_vh_gen_card_edge(
        &mut self,
        core: &VhConstCore,
        keep_result: bool,
        recv: StackSlot,
        expected: Option<StackSlot>,
        value: StackSlot,
    ) -> Vec<usize> {
        self.emit_gen_card_barrier(RAX, RDX, RAX, &[(RAX, recv), (RDX, value)]);
        if let Some(expected) = expected {
            self.load_slot_to_reg(R10, expected);
        }
        let (access, access_edges) = vh_const_access_bytes(core);
        let at = self.buf.pos();
        self.buf.emit(&access);
        let declines: Vec<usize> = access_edges.into_iter().map(|e| at + e).collect();
        if keep_result {
            self.buf.emit(&SP_VH_G1_KEEP_RESULT);
        }
        self.load_slot_to_reg(RAX, recv);
        self.load_slot_to_reg(RDX, value);
        self.emit_gen_card_barrier(RAX, RDX, RAX, &[]);
        if keep_result {
            self.buf.emit(&SP_VH_G1_RESTORE_RESULT);
        }
        if crate::x64::dbg_jitc_enabled() {
            eprintln!("[cratonvm-jitc] vh-const-inline (sp) generational card edge");
        }
        declines
    }
}

#[cfg(test)]
mod j27_vhcard_sp_edge_tests {
    use super::*;

    #[test]
    fn the_sp_card_edge_switch_is_default_on_and_rides_the_ir_edge_switch() {
        assert!(sp_vh_const_gen_card_edge_enabled());
        for key in [
            "CRATONVM_JIT_SP_VH_CONST_GEN_CARD_EDGE",
            "CRATONVM_JIT_VH_CONST_GEN_CARD_EDGE",
        ] {
            let off = cratonvm_types::flags::with_thread_overrides(
                &[(key, Some("0"))],
                sp_vh_const_gen_card_edge_enabled,
            );
            assert!(!off, "{key}=0 kills it");
        }
    }

    #[test]
    fn only_slots_that_survive_a_call_are_admitted() {
        assert!(survives_a_call(StackSlot::Frame(8)));
        assert!(survives_a_call(StackSlot::CalleeSaved(12)));
        assert!(!survives_a_call(StackSlot::Scratch(8)));
    }
}
