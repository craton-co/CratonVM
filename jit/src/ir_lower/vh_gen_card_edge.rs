// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 2 (lane VHCOMP; page
//! `docs/internal/jit/gen-vh-const-young-value-into-old-holder-declines-to-the-helper-FIXED-20261029.md`,
//! `CRATONVM_JIT_VH_CONST_GEN_CARD_EDGE`): under the generational heap, a
//! constant-`VarHandle` reference write (`set*`, CAS, compareAndExchange,
//! getAndSet) whose gate screen says a card may be owed -- an old holder, a
//! non-null value, old objects exist -- no longer declines to the ~700 ns
//! `VarHandle` helper. It pays the collector's own card-only barrier
//! (`jit_write_barrier(vm, holder, value)`, the call the gated `putfield`'s
//! POST edge makes) BEFORE and AFTER the access:
//!
//! ```text
//! edge:                                   ; RAX = holder, R10 = expected, RDX = value
//!   CALL write_barrier(vm, holder, value)  ; PRE: the holder's card dirty
//!   RAX / R10 / RDX = the operands again
//!   <vh_const_access_bytes>                ; RAX = the result
//!   [result home / register]
//!   CALL write_barrier(vm, holder, value)  ; POST
//!   [RAX = the result again] ; JMP hit
//! ```
//!
//! Why PRE and POST (`x64::objects::Compiler::emit_gen_card_barrier`'s
//! argument, point for point): a BUG-03 take-over may freeze this thread at any
//! instruction of the edge and hold it across a collection.
//!
//! * frozen before PRE's card store: the field still holds its old value, no
//!   edge to miss;
//! * frozen between PRE and the access: the collection consumes the card
//!   (the holder still holds its old value); after the resume the access runs
//!   and POST dirties the card again;
//! * frozen between the access and POST: the card is still the one PRE
//!   dirtied, so the collection scans the holder and sees the edge.
//!
//! The young value is in RDX (or the frame) throughout, a conservative root
//! of the frozen peer. The barrier is Rust code, which the take-over never
//! freezes, and never parks for a collection. A CAS that fails leaves a
//! spuriously dirty card (a card scan, nothing else). The SATB half is the
//! screen's, unchanged (it ran before the edge). A cast arm of the access may
//! still decline after PRE: the call then repeats the whole access, and the
//! card PRE dirtied costs one scan.

//! # The inline card check (j27 wave 4, lane VHCARD, `CRATONVM_JIT_VH_CONST_GEN_CARD_INLINE`)
//!
//! With the optimizing tier's inline card view (`Lowerer::ir_inline_card_view`)
//! the two calls become the gated `putfield` arm's PRE / POST card CHECK
//! (`Lowerer::emit_ir_putfield_card_arm` with `CRATONVM_JIT_POST_ONLY_CARD_MARK=0`),
//! and a call is left only on the edges that arm sends to its helper:
//!
//! ```text
//! edge:                                   ; RAX = holder, R10 = expected, RDX = value
//!   r11 = &view ; r10 = [r11 + old_base]
//!   cmp rax, r10 ; jb pre_call             ; flagged old outside the view
//!   cmp rax, [r11 + old_end] ; jae pre_call
//!   cmp rdx, r10 ; jb young
//!   cmp rdx, [r11 + old_end] ; jb no_card  ; old -> old: no card
//! young:
//!   r10 = &cards[(rax - old_base) >> CARD_SHIFT]
//!   PRE: cmp byte [r10], DIRTY ; je access ; clean: mark inline (or pre_call)
//!   jmp access
//! pre_call:
//!   CALL write_barrier(vm, holder, value) ; RAX / RDX = the operands again
//! access:
//!   R10 = expected ; <vh_const_access_bytes> ; [result home / register]
//!   r10 = holder ; r11 = &view ; range test (jb / jae post_call)
//!   r10 = &cards[...] ; cmp byte [r10], DIRTY ; jne post_call ; JMP hit
//! post_call:
//!   CALL write_barrier(vm, holder, value) ; [RAX = the result again] ; JMP hit
//! no_card:
//!   R10 = expected ; JMP <the screen's own access>
//! ```
//!
//! The PRE / POST argument above holds for the inline check unchanged: PRE
//! leaves the holder's card dirty (marked inline or by the call) before the
//! access, POST re-reads it after the access and dirties it again when a
//! collection cleaned it in between. POST re-forms the card from the holder's
//! home and the view's CURRENT words, range test included, because the access
//! overwrites R10 (a CAS needs `expected` there) and a collection inside the
//! freeze may move the old range; the holder itself does not move (a frozen
//! peer's registers are conservative roots). This is the classic PRE + POST
//! shape on purpose, never the post-only one: the post-only completion
//! (`x64::jit_post_only_card_mark_enabled`) needs the holder in a register
//! from the store to the card byte, which the access (RAX = its result) does
//! not keep.

use super::{
    ir_card_store_barrier_enabled, vh_const_access_bytes, IrCardMark, Lowerer, NodeId,
    VhConstCore, VhConstOp, CALL_ARG_REGS, R10, R11, RAX, RDX,
};
use super::{enc_frame_store, FrameAccess};

/// `CRATONVM_JIT_VH_CONST_GEN_CARD_INLINE` (j27 wave 4, lane VHCARD; perf,
/// **default ON**, `=0` keeps the two `write_barrier` calls of the card edge):
/// see "The inline card check" in the module comment. Read per site at
/// compile time.
pub(super) fn vh_const_gen_card_inline_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_VH_CONST_GEN_CARD_INLINE")
}

/// `CRATONVM_JIT_VH_CONST_GEN_CARD_EDGE` (j27 wave 2, lane VHCOMP; perf,
/// **default ON**, `=0` keeps the decline to the `VarHandle` helper): see the
/// module comment. Read per site at compile time.
pub(crate) fn vh_const_gen_card_edge_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_VH_CONST_GEN_CARD_EDGE")
}

impl Lowerer<'_> {
    /// May the IR prefix take the card edge for `core`? A gated (not G1)
    /// reference write, this compile has the generational card view and the
    /// card-only barrier, and the switch is on.
    pub(super) fn vh_gen_card_edge_admitted(&self, core: &VhConstCore) -> bool {
        core.gates.is_some()
            && core.g1.is_none()
            && core.kind == b'L'
            && core.op != VhConstOp::Get
            && self.ir_card_view.is_some()
            && self.card_write_barrier != 0
            && vh_const_gen_card_edge_enabled()
    }

    /// The edge of the module comment, entered with the operands in RAX / R10
    /// / RDX; joins `hit_at`. Returns the access's own decline fields (its
    /// cast screens), which the caller patches to the call.
    ///
    /// `plain_access` is the position of the gate screen's own access (its
    /// fall-through), which an old -> old value of the inline check rejoins.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit_ir_vh_gen_card_edge(
        &mut self,
        core: &VhConstCore,
        id: NodeId,
        (recv, expected, value): (NodeId, Option<NodeId>, NodeId),
        slot: Option<i32>,
        hit_at: usize,
        plain_access: usize,
    ) -> Vec<usize> {
        let ops = (recv, expected, value);
        let inline_view = self
            .ir_inline_card_view()
            .filter(|_| vh_const_gen_card_inline_enabled())
            .map(|(view, _, _, shift)| (view, shift));
        let declines = match inline_view {
            Some(view) => self.emit_ir_vh_card_inline_edge(core, id, ops, slot, view, plain_access),
            None => self.emit_ir_vh_card_call_edge(core, id, ops, slot),
        };
        self.emit_jmp_back_to(hit_at);
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] vh-const-inline (ir) n{id} generational card edge inline={}",
                inline_view.is_some()
            );
        }
        declines
    }

    /// The two-call edge of the module comment (VHCOMP's shape; the inline
    /// check's fallback). Ends with the result in RAX, before the jump to hit.
    fn emit_ir_vh_card_call_edge(
        &mut self,
        core: &VhConstCore,
        id: NodeId,
        (recv, expected, value): (NodeId, Option<NodeId>, NodeId),
        slot: Option<i32>,
    ) -> Vec<usize> {
        self.emit_ir_vh_card_barrier_call(recv, value);
        self.gp_load_value(RAX, recv);
        self.gp_load_value(RDX, value);
        let declines = self.emit_ir_vh_card_access(core, id, expected, slot);
        self.emit_ir_vh_card_barrier_call(recv, value);
        self.emit_ir_vh_card_result_again(id, slot);
        declines
    }

    /// "The inline card check" of the module comment. `view` = (the card
    /// view's address, CARD_SHIFT). Ends with the result in RAX, before the
    /// jump to hit.
    fn emit_ir_vh_card_inline_edge(
        &mut self,
        core: &VhConstCore,
        id: NodeId,
        (recv, expected, value): (NodeId, Option<NodeId>, NodeId),
        slot: Option<i32>,
        (view, shift): (usize, u8),
        plain_access: usize,
    ) -> Vec<usize> {
        // PRE: the operands are in RAX / R10 / RDX (the screens wrote RCX, R11).
        let mut pre_call: Vec<usize> = Vec::new();
        let no_card = self.emit_ir_vh_card_range_test(view, &mut pre_call);
        self.emit_ir_card_address_from_r10(view, shift);
        let store = ir_card_store_barrier_enabled().then_some((view, IrCardMark::Rax, shift));
        self.emit_ir_card_pre_check(&mut pre_call, store);
        let to_access = self.emit_jmp_rel32();
        for p in pre_call {
            self.patch_rel32_to_here(p);
        }
        self.emit_ir_vh_card_barrier_call(recv, value);
        self.gp_load_value(RAX, recv);
        self.gp_load_value(RDX, value);
        self.patch_rel32_to_here(to_access);
        let declines = self.emit_ir_vh_card_access(core, id, expected, slot);
        // POST, from the holder's home and the view's current words.
        self.gp_load_value(R10, recv);
        let post_call = self.emit_ir_vh_card_post_check(view, shift);
        let done = self.emit_jmp_rel32();
        for p in post_call {
            self.patch_rel32_to_here(p);
        }
        self.emit_ir_vh_card_barrier_call(recv, value);
        self.emit_ir_vh_card_result_again(id, slot);
        let out = self.emit_jmp_rel32();
        // old -> old: no card; the screen's own access (R10 = `expected`).
        self.patch_rel32_to_here(no_card);
        if let Some(expected) = expected {
            self.gp_load_value(R10, expected);
        }
        self.emit_jmp_back_to(plain_access);
        self.patch_rel32_to_here(done);
        self.patch_rel32_to_here(out);
        declines
    }

    /// The `putfield` arm's range filter on RAX (holder) / RDX (value), R11 =
    /// the view afterwards, R10 = the holder (the card's mark). A holder
    /// outside the view goes to `outside`; returns the old -> old edge.
    fn emit_ir_vh_card_range_test(&mut self, view: usize, outside: &mut Vec<usize>) -> usize {
        let old_base = crate::x64::CARD_VIEW_OLD_BASE.to_le_bytes();
        let old_end = crate::x64::CARD_VIEW_OLD_END.to_le_bytes();
        self.emit_mov_reg_imm64(R11, view as u64); // Cast: a baked data address
        self.buf.emit(&[0x4D, 0x8B, 0x93]); // MOV R10, [R11 + disp32]
        self.buf.emit(&old_base);
        self.buf.emit(&[0x4C, 0x39, 0xD0]); // CMP RAX, R10
        outside.push(self.emit_jcc_rel32(0x82)); // JB -> outside
        self.buf.emit(&[0x49, 0x3B, 0x83]); // CMP RAX, [R11 + disp32]
        self.buf.emit(&old_end);
        outside.push(self.emit_jcc_rel32(0x83)); // JAE -> outside
        self.buf.emit(&[0x4C, 0x39, 0xD2]); // CMP RDX, R10
        let young_value = self.emit_jcc_rel32(0x82); // JB -> young value
        self.buf.emit(&[0x49, 0x3B, 0x93]); // CMP RDX, [R11 + disp32]
        self.buf.emit(&old_end);
        let old_value = self.emit_jcc_rel32(0x82); // JB -> old -> old
        self.patch_rel32_to_here(young_value);
        self.buf.emit(&[0x49, 0x89, 0xC2]); // MOV R10, RAX
        old_value
    }

    /// POST on R10 = the holder: the range test against the view's current
    /// words, then `CMP byte [card], DIRTY; JNE`. Falls through when the card
    /// is dirty; returns the edges that owe the card-only call. Writes R10,
    /// R11 and the flags only (RAX = the result survives).
    fn emit_ir_vh_card_post_check(&mut self, view: usize, shift: u8) -> Vec<usize> {
        let mut call = Vec::new();
        self.emit_mov_reg_imm64(R11, view as u64); // Cast: a baked data address
        self.buf.emit(&[0x4D, 0x3B, 0x93]); // CMP R10, [R11 + disp32]
        self.buf
            .emit(&crate::x64::CARD_VIEW_OLD_BASE.to_le_bytes());
        call.push(self.emit_jcc_rel32(0x82)); // JB -> call
        self.buf.emit(&[0x4D, 0x3B, 0x93]); // CMP R10, [R11 + disp32]
        self.buf
            .emit(&crate::x64::CARD_VIEW_OLD_END.to_le_bytes());
        call.push(self.emit_jcc_rel32(0x83)); // JAE -> call
        self.emit_ir_card_address_from_r10(view, shift);
        let dirty = crate::x64::CARD_DIRTY_BYTE;
        self.buf.emit(&[0x41, 0x80, 0x3A, dirty]); // CMP byte [R10], DIRTY
        call.push(self.emit_jcc_rel32(0x85)); // JNE -> call (cleaned meanwhile)
        call
    }

    /// The access on RAX / RDX (R10 = `expected` loaded here), then the
    /// result to its register and home. Returns the access's decline fields.
    fn emit_ir_vh_card_access(
        &mut self,
        core: &VhConstCore,
        id: NodeId,
        expected: Option<NodeId>,
        slot: Option<i32>,
    ) -> Vec<usize> {
        if let Some(expected) = expected {
            self.gp_load_value(R10, expected);
        }
        let (access, edges) = vh_const_access_bytes(core);
        let at = self.buf.pos();
        self.buf.emit(&access);
        let declines: Vec<usize> = edges.into_iter().map(|e| at + e).collect();
        if let Some(slot) = slot {
            if let Some(reg) = self.assigned_gpr(id) {
                self.emit_mov_reg_reg64(reg, RAX);
            }
            let mut bytes = FrameAccess::new();
            enc_frame_store(RAX, slot, &mut bytes);
            self.buf.emit(bytes.as_slice());
        }
        declines
    }

    /// After a barrier call: RAX (and the result's register, which the call
    /// may have clobbered) = the result again, from its home.
    fn emit_ir_vh_card_result_again(&mut self, id: NodeId, slot: Option<i32>) {
        if let Some(slot) = slot {
            self.load_reg_from_frame(RAX, slot);
            if let Some(reg) = self.assigned_gpr(id) {
                self.emit_mov_reg_reg64(reg, RAX);
            }
        }
    }

    /// `jit_write_barrier(vm_ptr, holder, value)`: the collector's card-only
    /// barrier. Clobbers every caller-saved register, as the `Op::Call` this
    /// prefixes does.
    fn emit_ir_vh_card_barrier_call(&mut self, recv: NodeId, value: NodeId) {
        self.load_context(CALL_ARG_REGS[0]);
        self.gp_load_value(CALL_ARG_REGS[1], recv);
        self.gp_load_value(CALL_ARG_REGS[2], value);
        cratonvm_jit_api::assert_helper_call_shape!(
            "write_barrier",
            int_args = 3,
            returns_value = false
        );
        self.emit_mov_reg_imm64(RAX, self.card_write_barrier as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
    }
}

#[cfg(test)]
mod j27_vhcomp_gen_card_edge_tests {
    use super::*;

    #[test]
    fn the_gen_card_edge_switch_is_default_on_with_a_zero_kill() {
        assert!(vh_const_gen_card_edge_enabled());
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_VH_CONST_GEN_CARD_EDGE", Some("0"))],
            vh_const_gen_card_edge_enabled,
        );
        assert!(!off);
    }

    /// The gate screen's "card may be owed" exit is an edge only when asked:
    /// the default bytes keep it a decline, so the single-pass tier and the
    /// `Unsafe` arms are unchanged.
    #[test]
    fn the_card_edge_replaces_exactly_one_decline() {
        let g = super::super::VhRefGates {
            pre: 0x1000,
            post: 0x2000,
            floor: 0,
            mask: Some(0x40),
        };
        let mut plain = super::super::VhConstAsm::default();
        super::super::vh_ref_store_gate_screen_with(&mut plain, g, None, None);
        let mut edged = super::super::VhConstAsm::default();
        let mut edge = None;
        super::super::vh_ref_store_gate_screen_with(&mut edged, g, None, Some(&mut edge));
        assert_eq!(plain.b, edged.b, "same bytes, one jump retargeted");
        assert_eq!(plain.decline.len(), edged.decline.len() + 1);
        let edge = edge.expect("the edge");
        assert!(plain.decline.contains(&edge), "the retargeted jump is the owed-card decline");
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod j27_vhcard_inline_edge_tests {
    use super::super::{
        plan_slots, vh_const_core_bytes_gen_edge, Graph, IrType, Op, Schedule, SlotPlan,
        VhRefGates, VhRefValueCheck,
    };
    use crate::ir_schedule;
    use super::*;
    use crate::direct_helpers::UnsafeRmwOp;

    fn gated(op: VhConstOp) -> VhConstCore {
        VhConstCore {
            op,
            kind: b'L',
            disp: 24,
            class_id: 5,
            receiver_display: None,
            cid_off: 0,
            expected_check: VhRefValueCheck::Admitted,
            value_check: VhRefValueCheck::Admitted,
            gates: Some(VhRefGates {
                pre: 0x1000,
                post: 0x2000,
                floor: 0,
                mask: Some(0x40),
            }),
            g1: None,
            result_check: VhRefValueCheck::Admitted,
        }
    }

    const OPS: [VhConstOp; 5] = [
        VhConstOp::Set { volatile: false },
        VhConstOp::Set { volatile: true },
        VhConstOp::Rmw(UnsafeRmwOp::Cas),
        VhConstOp::Rmw(UnsafeRmwOp::Cae),
        VhConstOp::Rmw(UnsafeRmwOp::GetAndSet),
    ];

    #[test]
    fn the_inline_card_switch_is_default_on_with_a_zero_kill() {
        assert!(vh_const_gen_card_inline_enabled());
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_VH_CONST_GEN_CARD_INLINE", Some("0"))],
            vh_const_gen_card_inline_enabled,
        );
        assert!(!off);
    }

    /// The old -> old edge jumps to `at + core.len() - access.len()`: the
    /// screen's own access must be the core bytes' tail, for every write mode.
    #[test]
    fn the_screens_access_is_the_tail_of_the_core_bytes() {
        for op in OPS {
            let core = gated(op);
            let (bytes, _, edge) = vh_const_core_bytes_gen_edge(&core);
            let (access, _) = vh_const_access_bytes(&core);
            assert!(edge.is_some(), "{op:?}: a card edge");
            assert!(bytes.ends_with(&access), "{op:?}: the access ends the core");
        }
    }

    /// The edge's bytes: the inline form has the PRE and the POST card-byte
    /// compare and keeps both card-only calls (on their slow edges); `=0`, or
    /// no inline card view, is the two-call edge with no card compare.
    #[test]
    fn the_inline_edge_has_pre_and_post_card_compares_and_the_kill_has_none() {
        use cratonvm_gc::card_table::{CardTable, CardTableOptions};
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let param = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        g.exit = g.add(Op::Return, IrType::Void, vec![c0], Some(0));
        let schedule = ir_schedule::schedule(&g);
        let graph: &'static Graph = Box::leak(Box::new(g));
        let schedule: &'static Schedule = Box::leak(Box::new(schedule));
        let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
        let space: &'static mut [u64] = Box::leak(vec![0u64; 4096].into_boxed_slice());
        let ct: &'static CardTable = Box::leak(Box::new(CardTable::with_options(
            space.as_mut_ptr() as usize, // Cast: leaked address
            4096 * 8,
            CardTableOptions {
                summary: false,
                precise_ref_array_marks: false,
            },
        )));
        let view = ct.jit_card_view_addr();
        if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_INLINE_CARD_CHECK") {
            return;
        }
        let emit = |edits: &[(&str, Option<&str>)], op: VhConstOp| -> Vec<u8> {
            let mut all = vec![("CRATONVM_JIT_INLINE_CARD_MARK", Some("1"))];
            all.extend_from_slice(edits);
            cratonvm_types::flags::with_thread_overrides(&all, || {
                let mut lo = super::super::tests::lowerer_over(graph, schedule, plan, 8192);
                lo.ir_card_view = Some((view, false));
                lo.card_write_barrier = 0x1000;
                let before = lo.buf.pos();
                let ops = (param, Some(param), param);
                lo.emit_ir_vh_gen_card_edge(&gated(op), param, ops, None, 0, 0);
                lo.buf.as_slice()[before..].to_vec()
            })
        };
        let count = |b: &[u8], n: &[u8]| b.windows(n.len()).filter(|w| *w == n).count();
        let card_cmp = [0x41, 0x80, 0x3A, crate::x64::CARD_DIRTY_BYTE];
        let call = [0xFF, 0xD0];
        for op in OPS {
            for store in ["0", "1"] {
                let on = emit(&[("CRATONVM_JIT_CARD_STORE_BARRIER", Some(store))], op);
                assert_eq!(count(&on, &card_cmp), 2, "{op:?} store={store}: PRE and POST");
                assert_eq!(count(&on, &call), 2, "{op:?}: the two slow-edge calls");
            }
            let off = emit(&[("CRATONVM_JIT_VH_CONST_GEN_CARD_INLINE", Some("0"))], op);
            assert_eq!(count(&off, &card_cmp), 0, "{op:?}: =0 is the two-call edge");
            assert_eq!(count(&off, &call), 2);
            let no_view = emit(&[("CRATONVM_JIT_IR_INLINE_CARD_CHECK", Some("0"))], op);
            assert_eq!(no_view, off, "{op:?}: no inline card view, the two-call edge");
        }
    }
}
