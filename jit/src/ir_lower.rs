// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lower the scheduled IR graph to x86-64 machine code.
//!
//! Walks the scheduled basic blocks, emits native instructions for each
//! IR node, and patches forward branches.

use std::collections::HashMap;

use super::ir::{Graph, IrType, NodeId, Op, SafepointSnapshot, NO_NODE};
use super::ir_schedule::Schedule;
use super::{CompiledMethod, ExecutableBuffer};
use crate::deopt::{
    ir_deopt_entry, DeoptAction, DeoptReason, DeoptimizationPoint, FrameState, FrameValue,
};

// Argument registers for the deopt trampoline's call to `ir_deopt_entry`
// (`fn(point, rbp)`), per platform ABI.
#[cfg(target_os = "windows")]
const DEOPT_ARG0: u8 = 1; // RCX
#[cfg(target_os = "windows")]
const DEOPT_ARG1: u8 = 2; // RDX
#[cfg(not(target_os = "windows"))]
const DEOPT_ARG0: u8 = 7; // RDI
#[cfg(not(target_os = "windows"))]
const DEOPT_ARG1: u8 = 6; // RSI

/// Bytes of caller shadow space reserved above `rsp` for the deopt stub's
/// `call ir_deopt_entry` (Win64 requires 32; harmless on SysV). Kept clear of
/// spill slots by `alloc_slot`. See its doc comment.
const DEOPT_SHADOW_SPACE: i32 = 32;

// x86-64 register constants
#[allow(dead_code)]
const RAX: u8 = 0;
#[allow(dead_code)]
const RCX: u8 = 1;
#[allow(dead_code)]
const RDX: u8 = 2;

// ── Lowering state ───────────────────────────────────────────────────

struct Lowerer<'a> {
    graph: &'a Graph,
    schedule: &'a Schedule,
    buf: ExecutableBuffer,
    /// Maps NodeId → frame offset where its result is stored.
    node_slot: Vec<i32>,
    /// Next available frame spill offset.
    next_spill: i32,
    /// Maps block index → native code offset (for branch patching).
    block_offsets: Vec<usize>,
    /// Forward branch patches: (native_offset_of_rel32, target_block_idx).
    branch_patches: Vec<(usize, usize)>,
    /// Number of parameter slots.
    num_params: usize,
    /// Number of local variable slots.
    _num_locals: usize,
    /// Frame size (aligned).
    frame_size: i32,
    /// real-frame-deopt: bytecode pc → earliest native code offset emitted
    /// for that bci. Populated as nodes are lowered; used to anchor each
    /// safepoint snapshot to a native offset for `DeoptimizationPoint`.
    bci_native: HashMap<usize, usize>,
    /// real-frame-deopt: native offsets of `JMP rel32` instructions emitted by
    /// failed guards that must be patched to jump to the shared deopt stub.
    deopt_stub_patches: Vec<usize>,
    /// real-frame-deopt: boxed deopt points whose stable addresses are baked
    /// as imm64 into guard code. Moved into the `CompiledMethod` so the code's
    /// raw pointers stay valid for the method's (retained) lifetime.
    deopt_boxes: Vec<Box<DeoptimizationPoint>>,
}

impl<'a> Lowerer<'a> {
    fn new(
        graph: &'a Graph,
        schedule: &'a Schedule,
        buf: ExecutableBuffer,
        num_params: usize,
        num_locals: usize,
        max_nodes: usize,
    ) -> Self {
        // Frame layout: [RBP-8] = first slot, etc.
        // Reserve slots for locals + max_nodes spill slots + shadow space.
        // The 16-byte tail above the shadow region holds in-frame stack args
        // for any helper called without `emit_stack_arg_setup`; see the
        // matching comment in `x64.rs` (Compiler::new) for the worst-case
        // 6-arg `jit_invoke_virtual_mic` site that motivates 16 (not 8).
        let locals_size = (num_locals as i32) * 8;
        let spill_size = (max_nodes as i32) * 8;
        let shadow = 32i32;
        let stack_arg_reserve = 16i32;
        let total = locals_size + spill_size + shadow + stack_arg_reserve;
        let frame_size = (total + 15) & !15;

        Lowerer {
            graph,
            schedule,
            buf,
            node_slot: vec![0; graph.nodes.len()],
            next_spill: (num_locals as i32 + 1) * 8,
            block_offsets: vec![0; schedule.blocks.len()],
            branch_patches: Vec::new(),
            num_params,
            _num_locals: num_locals,
            frame_size,
            bci_native: HashMap::new(),
            deopt_stub_patches: Vec::new(),
            deopt_boxes: Vec::new(),
        }
    }

    /// Allocate a frame slot for a node result.
    /// Panics if the spill offset exceeds the allocated frame capacity.
    ///
    /// real-frame-deopt (#6): the deopt stub calls `ir_deopt_entry` while the
    /// frame is live, with `rsp = rbp - frame_size`. The Win64 ABI requires 32
    /// bytes of caller shadow space at `[rsp, rsp+32)` — i.e. frame offsets
    /// `(frame_size-32 .. frame_size]`. A spill slot at offset `o` occupies
    /// `[rbp-o, rbp-o+8)`; to keep it clear of the shadow region we cap
    /// `o <= frame_size - DEOPT_SHADOW_SPACE`. `frame_size` already budgets the
    /// 32-byte shadow (plus a 16-byte stack-arg reserve), so this never rejects
    /// a method the old `o < frame_size` bound accepted.
    fn alloc_slot(&mut self, id: NodeId) -> i32 {
        let offset = self.next_spill;
        assert!(
            offset <= self.frame_size - DEOPT_SHADOW_SPACE,
            "JIT lowerer: spill offset {} exceeds frame capacity {} \
             (less {}-byte deopt-call shadow reserve)",
            offset,
            self.frame_size,
            DEOPT_SHADOW_SPACE,
        );
        self.next_spill += 8;
        self.node_slot[id as usize] = offset;
        offset
    }

    /// Get the frame offset for a node's result (must have been allocated).
    fn slot_of(&self, id: NodeId) -> i32 {
        self.node_slot[id as usize]
    }

    // ── Phi resolution (BUG FIX [jit-irlower #2]) ────────────────────────
    //
    // Previously `Op::Phi` only called `alloc_slot` with the comment
    // "predecessors will write it" — but no code ever emitted those writes,
    // so a phi read an uninitialised frame slot. We now perform a standard
    // edge-split parallel copy: before a predecessor block branches to a
    // merge/region successor, store each incoming phi-argument value into
    // the corresponding phi's slot.

    /// Reserve a frame slot for every `Op::Phi` up front.
    ///
    /// A forward branch can target a merge block whose phis have not yet
    /// been lowered, so the destination slots must exist before any edge
    /// copy is emitted. Phis are skipped by `lower_data_node`.
    fn prealloc_phi_slots(&mut self) {
        for id in 0..self.graph.nodes.len() {
            if matches!(self.graph.nodes[id].op, Op::Phi) {
                self.alloc_slot(id as NodeId);
            }
        }
    }

    /// Resolve the block that produces control token `ctrl` by walking up
    /// control inputs until we reach a node that heads some block.
    ///
    /// Merge predecessor `k` records `self.ctrl` (the predecessor's live
    /// control node) as `merge.inputs[k]`; that token is either a block
    /// head directly (goto fall-through) or a `Proj` off an `If` (block
    /// head). Walking control inputs makes the mapping robust to either.
    fn block_of_ctrl(&self, mut ctrl: NodeId) -> Option<usize> {
        for _ in 0..self.graph.nodes.len() {
            if ctrl == NO_NODE {
                return None;
            }
            let blk = self.schedule.node_to_block[ctrl as usize];
            if blk != usize::MAX && self.schedule.blocks[blk].ctrl == ctrl {
                return Some(blk);
            }
            // Step up the control chain (first input is the control edge for
            // Proj/If/Merge-derived nodes).
            let node = &self.graph.nodes[ctrl as usize];
            match node.inputs.first() {
                Some(&next) if next != ctrl => ctrl = next,
                _ => return None,
            }
        }
        None
    }

    /// Emit the parallel-copy stores for every phi at `succ_block` whose
    /// merge has `pred_block` as the source for that phi argument.
    ///
    /// Each copy loads the incoming SSA value (already spilled by the
    /// predecessor block, which is lowered before its terminator) into RAX
    /// and stores it into the phi's reserved slot. The phi value sources
    /// are predecessor-side snapshots, never this merge's own phis, so the
    /// copies have no read-after-write cycle and a single scratch register
    /// is sufficient.
    fn emit_phi_copies(&mut self, pred_block: usize, succ_block: usize) {
        let merge_ctrl = self.schedule.blocks[succ_block].ctrl;
        // Only Merge/Region blocks carry phis tied to incoming edges.
        if !matches!(
            self.graph.nodes[merge_ctrl as usize].op,
            Op::Merge | Op::Region
        ) {
            return;
        }

        // Gather (phi_slot, value_id) pairs first to avoid borrowing `self`
        // immutably while emitting (which borrows `self` mutably).
        let mut copies: Vec<(i32, i32)> = Vec::new();
        for id in 0..self.graph.nodes.len() {
            let node = &self.graph.nodes[id];
            if !matches!(node.op, Op::Phi) {
                continue;
            }
            // Only value phis materialise a frame slot; memory/control phis
            // are bookkeeping tokens with no machine value to copy.
            if matches!(node.ty, IrType::Memory | IrType::Control | IrType::Void) {
                continue;
            }
            // phi.inputs = [merge, val_0, val_1, …]
            if node.inputs.first().copied() != Some(merge_ctrl) {
                continue;
            }
            // merge.inputs[k] is the control token for phi value k (= input k+1).
            let merge_node = &self.graph.nodes[merge_ctrl as usize];
            for (k, &ctrl_in) in merge_node.inputs.iter().enumerate() {
                if self.block_of_ctrl(ctrl_in) != Some(pred_block) {
                    continue;
                }
                if let Some(&val_id) = node.inputs.get(k + 1) {
                    if val_id != NO_NODE {
                        copies.push((self.slot_of(id as NodeId), self.slot_of(val_id)));
                    }
                }
            }
        }

        for (dst_slot, src_slot) in copies {
            self.load_to_rax(src_slot);
            self.store_rax(dst_slot);
        }
    }

    // ── Code emission helpers ────────────────────────────────────────

    fn emit_prologue(&mut self) {
        // push rbp
        self.buf.emit_byte(0x55);
        // mov rbp, rsp
        self.buf.emit(&[0x48, 0x89, 0xE5]);
        // sub rsp, frame_size
        self.buf.emit(&[0x48, 0x81, 0xEC]);
        self.buf.emit(&self.frame_size.to_le_bytes());

        // Store params from ABI registers to local frame slots.
        // Windows: RCX, RDX, R8, R9.  SysV: RDI, RSI, RDX, RCX.
        #[cfg(target_os = "windows")]
        let abi_regs: &[u8] = &[RCX, RDX, 8, 9]; // RCX, RDX, R8, R9
        #[cfg(not(target_os = "windows"))]
        let abi_regs: &[u8] = &[7, 6, RDX, RCX, 8, 9]; // RDI, RSI, RDX, RCX, R8, R9

        for i in 0..self.num_params.min(abi_regs.len()) {
            let reg = abi_regs[i];
            let offset = ((i as i32) + 1) * 8; // local_offset(i)
            let neg = -(offset as i32);
            // MOV [RBP - offset], reg
            let mut prefix = 0x48u8; // REX.W
            if reg >= 8 {
                prefix |= 0x04; // REX.R
            }
            self.buf.emit_byte(prefix);
            self.buf.emit_byte(0x89);
            // Prefer the shorter disp8 form when neg fits in i8 — for the
            // first 16 params we know neg ∈ [-128, -8], well within range.
            if (i8::MIN as i32..=i8::MAX as i32).contains(&neg) {
                // mod=01, reg=reg&7, r/m=RBP(101) → 0x45 | (reg<<3)
                self.buf.emit_byte(0x45 | ((reg & 7) << 3));
                self.buf.emit_byte(neg as u8);
            } else {
                // mod=10, disp32 fallback
                self.buf.emit_byte(0x85 | ((reg & 7) << 3));
                self.buf.emit(&neg.to_le_bytes());
            }
        }
    }

    fn emit_epilogue(&mut self) {
        // add rsp, frame_size
        self.buf.emit(&[0x48, 0x81, 0xC4]);
        self.buf.emit(&self.frame_size.to_le_bytes());
        // pop rbp
        self.buf.emit_byte(0x5D);
        // ret
        self.buf.emit_byte(0xC3);
    }

    /// MOV RAX, [RBP - offset]
    ///
    /// Emits the shorter disp8 form (mod=01, 4 bytes) when `neg`
    /// fits in a signed 8-bit value, falling back to disp32 (mod=10,
    /// 7 bytes) otherwise. Most spill slots for typical methods sit
    /// within ±128 bytes of RBP, so the disp8 form is the common case
    /// and saves 3 bytes per frame access.
    fn load_to_rax(&mut self, offset: i32) {
        let neg = -(offset as i32);
        if (i8::MIN as i32..=i8::MAX as i32).contains(&neg) {
            // 48 8B 45 disp8  — mod=01, reg=RAX(0), r/m=RBP(101)
            self.buf.emit(&[0x48, 0x8B, 0x45, neg as u8]);
        } else {
            // 48 8B 85 disp32 — mod=10
            self.buf.emit(&[0x48, 0x8B, 0x85]);
            self.buf.emit(&neg.to_le_bytes());
        }
    }

    /// MOV RCX, [RBP - offset]
    fn load_to_rcx(&mut self, offset: i32) {
        let neg = -(offset as i32);
        if (i8::MIN as i32..=i8::MAX as i32).contains(&neg) {
            // 48 8B 4D disp8  — mod=01, reg=RCX(1), r/m=RBP(101)
            self.buf.emit(&[0x48, 0x8B, 0x4D, neg as u8]);
        } else {
            // 48 8B 8D disp32 — mod=10
            self.buf.emit(&[0x48, 0x8B, 0x8D]);
            self.buf.emit(&neg.to_le_bytes());
        }
    }

    /// MOV [RBP - offset], RAX
    fn store_rax(&mut self, offset: i32) {
        let neg = -(offset as i32);
        if (i8::MIN as i32..=i8::MAX as i32).contains(&neg) {
            // 48 89 45 disp8  — mod=01, reg=RAX(0), r/m=RBP(101)
            self.buf.emit(&[0x48, 0x89, 0x45, neg as u8]);
        } else {
            // 48 89 85 disp32 — mod=10
            self.buf.emit(&[0x48, 0x89, 0x85]);
            self.buf.emit(&neg.to_le_bytes());
        }
    }

    /// MOV RAX, imm64
    fn emit_mov_rax_imm64(&mut self, val: i64) {
        if val >= i32::MIN as i64 && val <= i32::MAX as i64 {
            // MOV EAX, imm32 (sign-extended to 64-bit)
            if val >= 0 && val <= u32::MAX as i64 {
                self.buf.emit_byte(0xB8);
                self.buf.emit(&(val as u32).to_le_bytes());
            } else {
                // MOV RAX, imm32 sign-extended
                self.buf.emit(&[0x48, 0xC7, 0xC0]);
                self.buf.emit(&(val as i32).to_le_bytes());
            }
        } else {
            // MOV RAX, imm64
            self.buf.emit(&[0x48, 0xB8]);
            self.buf.emit(&val.to_le_bytes());
        }
    }

    /// MOV reg, imm64 (REX.W [+ REX.B for r8–r15]).
    fn emit_mov_reg_imm64(&mut self, reg: u8, val: u64) {
        let rex = 0x48 | if reg >= 8 { 0x01 } else { 0 }; // REX.W (+REX.B)
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0xB8 + (reg & 7));
        self.buf.emit(&val.to_le_bytes());
    }

    /// MOV reg, RBP (REX.W [+ REX.B]).
    fn emit_mov_reg_rbp(&mut self, reg: u8) {
        let rex = 0x48 | if reg >= 8 { 0x01 } else { 0 }; // REX.W (+REX.B for r/m)
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x89);
        // ModRM: mod=11, reg=RBP(5), r/m=reg → 0xC0 | (5<<3) | (reg&7)
        self.buf.emit_byte(0xE8 | (reg & 7));
    }

    // ── Node lowering ────────────────────────────────────────────────

    fn lower_block(&mut self, block_idx: usize) {
        self.block_offsets[block_idx] = self.buf.pos();

        let block = &self.schedule.blocks[block_idx];

        // Emit data nodes
        for &node_id in &block.nodes {
            self.lower_data_node(node_id);
        }

        // Emit terminator
        if let Some(term) = block.terminator {
            self.lower_terminator(term, block_idx);
        } else {
            // No explicit terminator: this is a goto / fall-through edge into
            // a Merge/Region. BUG FIX [jit-irlower #2]: emit the edge's phi
            // copies before transferring control, then jump to the successor
            // explicitly (block emission order is not guaranteed to place the
            // successor physically next).
            let succ = self.schedule.blocks[block_idx].successors.first().copied();
            if let Some(succ_block) = succ {
                self.emit_phi_copies(block_idx, succ_block);
                self.buf.emit_byte(0xE9); // JMP succ_block
                let patch_pos = self.buf.pos();
                self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                self.branch_patches.push((patch_pos, succ_block));
            }
        }
    }

    fn lower_data_node(&mut self, id: NodeId) {
        // real-frame-deopt: anchor the node's bytecode pc to the earliest
        // native offset emitted for it, so safepoint snapshots can be keyed
        // by native offset. `self.graph` is a `&'a Graph`, so reading
        // `bytecode_pc` here does not borrow `self`.
        if let Some(pc) = self.graph.nodes[id as usize].bytecode_pc {
            let here = self.buf.pos();
            self.bci_native
                .entry(pc)
                .and_modify(|e| {
                    if here < *e {
                        *e = here;
                    }
                })
                .or_insert(here);
        }

        let node = &self.graph.nodes[id as usize];
        match &node.op {
            Op::Const(val) => {
                let slot = self.alloc_slot(id);
                self.emit_mov_rax_imm64(*val);
                self.store_rax(slot);
            }
            Op::Param(idx) => {
                let slot = self.alloc_slot(id);
                // Params were stored to local frame slots by the prologue.
                // local_offset(i) = (i + 1) * 8
                let param_offset = ((*idx as i32) + 1) * 8;
                self.load_to_rax(param_offset);
                self.store_rax(slot);
            }
            Op::Add => {
                let slot = self.alloc_slot(id);
                self.load_to_rax(self.slot_of(node.inputs[0]));
                self.load_to_rcx(self.slot_of(node.inputs[1]));
                if node.ty == IrType::Int {
                    // ADD EAX, ECX
                    self.buf.emit(&[0x01, 0xC8]);
                } else {
                    // ADD RAX, RCX
                    self.buf.emit(&[0x48, 0x01, 0xC8]);
                }
                self.store_rax(slot);
            }
            Op::Sub => {
                let slot = self.alloc_slot(id);
                self.load_to_rax(self.slot_of(node.inputs[0]));
                self.load_to_rcx(self.slot_of(node.inputs[1]));
                if node.ty == IrType::Int {
                    // SUB EAX, ECX
                    self.buf.emit(&[0x29, 0xC8]);
                } else {
                    // SUB RAX, RCX
                    self.buf.emit(&[0x48, 0x29, 0xC8]);
                }
                self.store_rax(slot);
            }
            Op::Mul => {
                let slot = self.alloc_slot(id);
                self.load_to_rax(self.slot_of(node.inputs[0]));
                self.load_to_rcx(self.slot_of(node.inputs[1]));
                if node.ty == IrType::Int {
                    // IMUL EAX, ECX
                    self.buf.emit(&[0x0F, 0xAF, 0xC1]);
                } else {
                    // IMUL RAX, RCX
                    self.buf.emit(&[0x48, 0x0F, 0xAF, 0xC1]);
                }
                self.store_rax(slot);
            }
            Op::Div => {
                let slot = self.alloc_slot(id);
                self.load_to_rax(self.slot_of(node.inputs[0]));
                self.load_to_rcx(self.slot_of(node.inputs[1]));
                if node.ty == IrType::Int {
                    // CDQ (sign-extend EAX → EDX:EAX)
                    self.buf.emit_byte(0x99);
                    // IDIV ECX
                    self.buf.emit(&[0xF7, 0xF9]);
                } else {
                    // CQO (sign-extend RAX → RDX:RAX)
                    self.buf.emit(&[0x48, 0x99]);
                    // IDIV RCX
                    self.buf.emit(&[0x48, 0xF7, 0xF9]);
                }
                self.store_rax(slot);
            }
            Op::Rem => {
                let slot = self.alloc_slot(id);
                self.load_to_rax(self.slot_of(node.inputs[0]));
                self.load_to_rcx(self.slot_of(node.inputs[1]));
                if node.ty == IrType::Int {
                    self.buf.emit_byte(0x99); // CDQ
                    self.buf.emit(&[0xF7, 0xF9]); // IDIV ECX
                } else {
                    self.buf.emit(&[0x48, 0x99]); // CQO
                    self.buf.emit(&[0x48, 0xF7, 0xF9]); // IDIV RCX
                }
                // Remainder is in RDX; move to RAX
                // MOV RAX, RDX
                self.buf.emit(&[0x48, 0x89, 0xD0]);
                self.store_rax(slot);
            }
            Op::Neg => {
                let slot = self.alloc_slot(id);
                self.load_to_rax(self.slot_of(node.inputs[0]));
                if node.ty == IrType::Int {
                    // NEG EAX
                    self.buf.emit(&[0xF7, 0xD8]);
                } else {
                    // NEG RAX
                    self.buf.emit(&[0x48, 0xF7, 0xD8]);
                }
                self.store_rax(slot);
            }
            Op::And => {
                let slot = self.alloc_slot(id);
                self.load_to_rax(self.slot_of(node.inputs[0]));
                self.load_to_rcx(self.slot_of(node.inputs[1]));
                // AND RAX, RCX
                self.buf.emit(&[0x48, 0x21, 0xC8]);
                self.store_rax(slot);
            }
            Op::Or => {
                let slot = self.alloc_slot(id);
                self.load_to_rax(self.slot_of(node.inputs[0]));
                self.load_to_rcx(self.slot_of(node.inputs[1]));
                // OR RAX, RCX
                self.buf.emit(&[0x48, 0x09, 0xC8]);
                self.store_rax(slot);
            }
            Op::Xor => {
                let slot = self.alloc_slot(id);
                self.load_to_rax(self.slot_of(node.inputs[0]));
                self.load_to_rcx(self.slot_of(node.inputs[1]));
                // XOR RAX, RCX
                self.buf.emit(&[0x48, 0x31, 0xC8]);
                self.store_rax(slot);
            }
            Op::Shl => {
                let slot = self.alloc_slot(id);
                self.load_to_rax(self.slot_of(node.inputs[0]));
                self.load_to_rcx(self.slot_of(node.inputs[1]));
                if node.ty == IrType::Int {
                    // SHL EAX, CL
                    self.buf.emit(&[0xD3, 0xE0]);
                } else {
                    // SHL RAX, CL
                    self.buf.emit(&[0x48, 0xD3, 0xE0]);
                }
                self.store_rax(slot);
            }
            Op::Shr => {
                let slot = self.alloc_slot(id);
                self.load_to_rax(self.slot_of(node.inputs[0]));
                self.load_to_rcx(self.slot_of(node.inputs[1]));
                if node.ty == IrType::Int {
                    // SAR EAX, CL
                    self.buf.emit(&[0xD3, 0xF8]);
                } else {
                    // SAR RAX, CL
                    self.buf.emit(&[0x48, 0xD3, 0xF8]);
                }
                self.store_rax(slot);
            }
            Op::UShr => {
                let slot = self.alloc_slot(id);
                self.load_to_rax(self.slot_of(node.inputs[0]));
                self.load_to_rcx(self.slot_of(node.inputs[1]));
                if node.ty == IrType::Int {
                    // SHR EAX, CL
                    self.buf.emit(&[0xD3, 0xE8]);
                } else {
                    // SHR RAX, CL
                    self.buf.emit(&[0x48, 0xD3, 0xE8]);
                }
                self.store_rax(slot);
            }
            Op::Cmp(cc) => {
                let slot = self.alloc_slot(id);
                self.load_to_rax(self.slot_of(node.inputs[0]));
                self.load_to_rcx(self.slot_of(node.inputs[1]));
                // CMP EAX, ECX
                self.buf.emit(&[0x39, 0xC8]);
                // SETcc AL — three bytes: `0F 9x C0`.
                //
                // BUG FIX [jit-irlower #1]: `x64_cc()` returns the *near-Jcc*
                // second byte (0x84..=0x8F, i.e. `0F 8x`). The SETcc second
                // byte is the Jcc value PLUS 0x10 (0x94..=0x9F, i.e. `0F 9x`),
                // NOT minus. The old `- 0x10` produced `0F 7x` (MMX
                // PCMPEQB/etc.), which never sets AL. Use `+ 0x10`.
                //
                // BUG FIX [jit-irlower #3]: the ModRM byte (`0xC0`, selecting
                // AL) was MISSING — only `0F 9x` was emitted. SETcc is a
                // /digit form and REQUIRES a ModRM operand byte; without it the
                // instruction stream desynced (the following `0F B6` MOVZX got
                // partly consumed as SETcc's ModRM) and AL/RAX were never
                // written, so RAX kept the first operand loaded above. That is
                // why `Op::Cmp` returned an input (`a`) instead of the 0/1
                // boolean, and why a Cmp feeding an `If` branched on `a` and
                // always took the else edge. Emit the full `0F 9x C0`.
                self.buf.emit(&[0x0F, cc.x64_cc() + 0x10, 0xC0]); // SETcc AL
                                                                  // MOVZX EAX, AL
                self.buf.emit(&[0x0F, 0xB6, 0xC0]);
                self.store_rax(slot);
            }
            Op::I2L => {
                let slot = self.alloc_slot(id);
                self.load_to_rax(self.slot_of(node.inputs[0]));
                // MOVSXD RAX, EAX
                self.buf.emit(&[0x48, 0x63, 0xC0]);
                self.store_rax(slot);
            }
            Op::L2I => {
                let slot = self.alloc_slot(id);
                self.load_to_rax(self.slot_of(node.inputs[0]));
                // MOV EAX, EAX (zero-extend / truncate to 32 bits)
                self.buf.emit(&[0x89, 0xC0]);
                self.store_rax(slot);
            }
            Op::Phi => {
                // Phi nodes are resolved by predecessors storing their
                // incoming value into the phi's slot at the controlling
                // block edge. The slot is reserved up front by
                // `prealloc_phi_slots` (a forward branch may target a phi
                // whose block has not yet been lowered), and the parallel
                // copy is emitted by `emit_phi_copies` (see BUG FIX
                // [jit-irlower #2]) just before each predecessor's branch
                // to this phi's merge block. Nothing to emit here.
            }
            Op::Guard { bci } => {
                // real-frame-deopt step 3: a speculative guard. If `cond`
                // (inputs[1]) is zero, transfer to the shared deopt stub which
                // reconstructs the interpreter frame for `bci` and returns the
                // deopt sentinel; otherwise fall through.
                let bci = *bci;
                let cond_slot = self.slot_of(node.inputs[1]);

                // Build + box the deopt point for this guard (stable address,
                // baked below). The point's frame state comes from the safepoint
                // snapshot recorded for `bci` during IR building.
                let frame_state = self.resolve_frame_state_for_bci(bci);
                let point = Box::new(DeoptimizationPoint {
                    native_offset: self.buf.pos() as u32,
                    bci: bci as u32,
                    reason: DeoptReason::UncommonTrap,
                    action: DeoptAction::Reinterpret,
                    speculation_id: 0,
                    frame_state,
                });
                let point_ptr = point.as_ref() as *const DeoptimizationPoint as u64;
                self.deopt_boxes.push(point);

                // cond → RAX; TEST EAX, EAX
                self.load_to_rax(cond_slot);
                self.buf.emit(&[0x85, 0xC0]);
                // JNZ continue (skip deopt when cond != 0): 0F 85 rel32
                self.buf.emit(&[0x0F, 0x85]);
                let jnz_patch = self.buf.pos();
                self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                // Deopt path: load the point pointer into arg0, JMP to stub.
                self.emit_mov_reg_imm64(DEOPT_ARG0, point_ptr);
                self.buf.emit_byte(0xE9); // JMP rel32 → deopt stub
                let jmp_patch = self.buf.pos();
                self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                self.deopt_stub_patches.push(jmp_patch);
                // continue: patch the JNZ to here (fall-through past the deopt).
                let cont = self.buf.pos();
                let rel = cont as i32 - (jnz_patch as i32 + 4);
                self.buf
                    .try_patch_i32(jnz_patch, rel)
                    .expect("guard JNZ patch in-bounds");
            }
            // Control and meta nodes — skip
            Op::Start | Op::Return | Op::If | Op::Merge | Op::Region | Op::Proj(_) | Op::Dead => {}
            // Unhandled — skip (bail in ir_compatible prevents reaching here)
            _ => {}
        }
    }

    fn lower_terminator(&mut self, term: NodeId, block_idx: usize) {
        let node = &self.graph.nodes[term as usize];
        match &node.op {
            Op::Return => {
                if node.inputs.len() > 1 {
                    // Has return value — move to RAX
                    let val_id = node.inputs[1];
                    if val_id != NO_NODE {
                        self.load_to_rax(self.slot_of(val_id));
                    }
                }
                self.emit_epilogue();
            }
            Op::If => {
                // Load condition into RAX
                let cond_id = node.inputs[1];
                self.load_to_rax(self.slot_of(cond_id));
                // TEST EAX, EAX  (does not disturb RAX; sets ZF)
                self.buf.emit(&[0x85, 0xC0]);

                // Snapshot successor block indices (immutable borrow ends
                // here so `emit_phi_copies` can borrow `self` mutably).
                let succ0 = self.schedule.blocks[block_idx].successors.first().copied();
                let succ1 = self.schedule.blocks[block_idx].successors.get(1).copied();

                match (succ0, succ1) {
                    (Some(true_block), Some(false_block)) => {
                        // BUG FIX [jit-irlower #2]: phi copies must execute on
                        // the edge actually taken, so the conditional branch
                        // splits the critical edges. Layout:
                        //   TEST; JE around_true;
                        //   <true-edge phi copies>; JMP true_block;
                        //   around_true: <false-edge phi copies>; JMP false_block;
                        //
                        // JE around_true (jump when condition == 0)
                        self.buf.emit(&[0x0F, 0x84]);
                        let je_patch = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                        // Taken (true) edge.
                        self.emit_phi_copies(block_idx, true_block);
                        self.buf.emit_byte(0xE9); // JMP true_block
                        let jmp_true = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                        self.branch_patches.push((jmp_true, true_block));

                        // around_true: false edge. Patch the JE here.
                        let around_true = self.buf.pos();
                        let rel = around_true as i32 - (je_patch as i32 + 4);
                        self.buf
                            .try_patch_i32(je_patch, rel)
                            .expect("codegen patch in-bounds");
                        self.emit_phi_copies(block_idx, false_block);
                        self.buf.emit_byte(0xE9); // JMP false_block
                        let jmp_false = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                        self.branch_patches.push((jmp_false, false_block));
                    }
                    (Some(only_block), None) => {
                        // Degenerate single-successor If: copies are
                        // unconditional, then a plain JNE to the target
                        // (preserving the original taken-on-nonzero shape).
                        self.emit_phi_copies(block_idx, only_block);
                        self.buf.emit(&[0x0F, 0x85]); // JNE only_block
                        let patch_pos = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                        self.branch_patches.push((patch_pos, only_block));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn patch_branches(&mut self) {
        for &(patch_pos, target_block) in &self.branch_patches {
            let target_offset = self.block_offsets[target_block];
            let rel32 = target_offset as i32 - (patch_pos as i32 + 4);
            self.buf
                .try_patch_i32(patch_pos, rel32)
                .expect("codegen patch in-bounds");
        }
    }

    // ── Deopt frame-state resolution (real-frame-deopt step 2) ───────────

    /// Resolve the machine location of a single IR value into a `FrameValue`.
    ///
    /// This naive single-scratch lowerer spills every node result to a frame
    /// slot (`node_slot`), so a live SSA value is found either as a constant
    /// (encoded directly) or at a frame spill slot — never in a register.
    /// That is exactly the cleanest first cut for deopt: every non-constant
    /// resolves to a `StackSlot`, and the VM reads it from the native stack.
    ///
    /// Convention: `StackSlot(off)` is read by the VM as `*(rbp + off)`
    /// (matching `deopt.rs`). The lowerer stores results at `[rbp - slot]`
    /// with `slot > 0`, so we encode the *negative* offset here.
    fn frame_value_for(&self, node_id: NodeId) -> FrameValue {
        if node_id == NO_NODE {
            return FrameValue::Undefined;
        }
        let node = &self.graph.nodes[node_id as usize];
        match node.op {
            // Integer / long constants need no machine location.
            Op::Const(v) => FrameValue::Int(v),
            // Float / double constant bits.
            Op::ConstF(bits) => FrameValue::Float(bits),
            Op::Param(idx) => {
                // The prologue stored param `idx` at `[rbp - (idx+1)*8]`. If
                // the Param node was also scheduled it has its own spill slot
                // holding the same value; prefer that, else the prologue slot.
                let slot = self.node_slot[node_id as usize];
                let off = if slot != 0 {
                    slot
                } else {
                    ((idx as i32) + 1) * 8
                };
                FrameValue::StackSlot(-off)
            }
            _ => {
                let slot = self.node_slot[node_id as usize];
                if slot != 0 {
                    FrameValue::StackSlot(-slot)
                } else {
                    // No machine location assigned (unscheduled / dead in this
                    // naive lowerer). A real resolver would never see this for
                    // a value that is live at the safepoint; first-cut fallback.
                    FrameValue::Undefined
                }
            }
        }
    }

    /// Resolve the `FrameState` for `bci` from its recorded safepoint
    /// snapshot. Falls back to an empty frame if no snapshot exists (e.g. a
    /// hand-built graph that did not register one) — the resume bci is still
    /// carried so the deopt is well-formed.
    fn resolve_frame_state_for_bci(&self, bci: usize) -> FrameState {
        match self.graph.safepoints.iter().find(|s| s.bci == bci) {
            Some(sp) => self.resolve_frame_state(sp),
            None => FrameState {
                method_key: String::new(),
                bci: bci as u32,
                locals: Vec::new(),
                stack: Vec::new(),
                monitors: Vec::new(),
                caller: None,
            },
        }
    }

    /// Emit the single shared deopt stub (if any guard jumps to it) and patch
    /// every guard's `JMP` to it. The stub expects the failing guard's
    /// `DeoptimizationPoint` pointer already in `DEOPT_ARG0`; it loads `rbp`
    /// into `DEOPT_ARG1`, calls `ir_deopt_entry`, and returns its result (the
    /// `i64::MIN` deopt sentinel) via the normal epilogue.
    fn emit_deopt_stub(&mut self) {
        if self.deopt_stub_patches.is_empty() {
            return;
        }
        let stub_off = self.buf.pos();
        // mov arg1, rbp
        self.emit_mov_reg_rbp(DEOPT_ARG1);
        // mov rax, ir_deopt_entry ; call rax
        let fn_addr = ir_deopt_entry as *const () as u64;
        self.emit_mov_reg_imm64(RAX, fn_addr);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        // Epilogue (RAX holds the sentinel returned by ir_deopt_entry).
        self.buf.emit(&[0x48, 0x81, 0xC4]); // add rsp, frame_size
        self.buf.emit(&self.frame_size.to_le_bytes());
        self.buf.emit_byte(0x5D); // pop rbp
        self.buf.emit_byte(0xC3); // ret

        let patches = std::mem::take(&mut self.deopt_stub_patches);
        for p in patches {
            let rel = stub_off as i32 - (p as i32 + 4);
            self.buf
                .try_patch_i32(p, rel)
                .expect("deopt JMP patch in-bounds");
        }
    }

    /// Build the interpreter `FrameState` for one safepoint snapshot.
    ///
    /// `method_key` is left to the VM caller to fill (the lowerer does not
    /// know it); deopt resume keys on the running `CompiledMethod`, not this
    /// string. It is recorded empty here.
    fn resolve_frame_state(&self, sp: &SafepointSnapshot) -> FrameState {
        FrameState {
            method_key: String::new(),
            bci: sp.bci as u32,
            locals: sp.locals.iter().map(|&n| self.frame_value_for(n)).collect(),
            stack: sp.stack.iter().map(|&n| self.frame_value_for(n)).collect(),
            monitors: Vec::new(),
            caller: None,
        }
    }

    /// Resolve every recorded safepoint snapshot into a `DeoptimizationPoint`,
    /// keyed by the native offset of its bci. Returns them sorted+deduped by
    /// `native_offset` so [`CompiledMethod::find_deopt_point`] can binary
    /// search. Snapshots whose bci emitted no machine code are skipped.
    fn build_deopt_points(&self) -> Vec<DeoptimizationPoint> {
        let mut points: Vec<DeoptimizationPoint> = Vec::with_capacity(self.graph.safepoints.len());
        for sp in &self.graph.safepoints {
            let native_offset = match self.bci_native.get(&sp.bci) {
                Some(&off) => off as u32,
                // bci produced no node / no machine code — nothing to anchor.
                None => continue,
            };
            points.push(DeoptimizationPoint {
                native_offset,
                bci: sp.bci as u32,
                // No speculation yet — these are plain resume points (step 2,
                // emit-and-discard). A real guard (step 3) sets its own reason.
                reason: DeoptReason::TransferToInterpreter,
                action: DeoptAction::Reinterpret,
                speculation_id: 0,
                frame_state: self.resolve_frame_state(sp),
            });
        }
        points.sort_by_key(|p| p.native_offset);
        points.dedup_by_key(|p| p.native_offset);
        points
    }
}

// ── Public entry point ───────────────────────────────────────────────

/// Lower the scheduled IR graph to x86-64 machine code.
///
/// TODO(round-8, HIGH from round-7 jit #9): no Loop-Invariant Code
/// Motion (LICM) for `Op::LoadField` / `Op::LoadStatic` on a
/// loop-invariant base. A loop like
///
///   for (int i = 0; i < n; i++) {
///       sum += this.scale * arr[i];   // getfield `this.scale` every iter
///   }
///
/// reloads `this.scale` on every iteration even though `this` is
/// loop-invariant and `scale` is effectively final. The IR currently
/// schedules the load inside the loop body; the lowerer faithfully
/// emits one load per iteration.
///
/// Desired round-8 behavior (implemented in `ir_optimize.rs` so this
/// lowerer sees a pre-hoisted graph):
///   1. After GVN, detect loops via a CFG back-edge scan.
///   2. Mark every `Op::Const`, `Op::Param`, and pure node whose
///      inputs are loop-invariant as loop-invariant.
///   3. Identify `Op::LoadField` / `Op::LoadStatic` where the base is
///      loop-invariant, the field is final, and no
///      `Op::StoreField` / `Op::StaticBarrier` in the loop body could
///      alias.
///   4. Move each such load to the loop pre-header block; rewrite
///      in-loop uses to the hoisted SSA value.
///
/// Estimated impact on JDK-shape workloads: 5-15% on getfield-heavy
/// inner loops; getstatic-of-final sees the largest wins because
/// alias-checking the load is trivial.
///
/// Deferred from this wave because the loop-detection pass and
/// alias-analysis lattice deserve their own session with dedicated
/// unit tests.
pub fn lower(
    graph: &Graph,
    schedule: &Schedule,
    num_params: usize,
    num_locals: usize,
) -> Option<CompiledMethod> {
    let estimated_size = graph.nodes.len() * 32 + 256;
    let buf = ExecutableBuffer::new(estimated_size.max(4096))?;

    let mut lowerer = Lowerer::new(
        graph,
        schedule,
        buf,
        num_params,
        num_locals,
        graph.nodes.len(),
    );

    // BUG FIX [jit-irlower #2]: reserve phi destination slots before any
    // block is lowered — a forward branch's edge copies (emit_phi_copies)
    // reference slots of phis in not-yet-lowered merge blocks.
    lowerer.prealloc_phi_slots();

    lowerer.emit_prologue();

    // Emit blocks in order
    for block_idx in 0..schedule.blocks.len() {
        lowerer.lower_block(block_idx);
    }

    // real-frame-deopt (step 3): emit the shared deopt stub after the method
    // body so failed guards can jump to it, then patch in-method branches.
    lowerer.emit_deopt_stub();

    lowerer.patch_branches();

    // real-frame-deopt (step 2): resolve recorded safepoint snapshots into
    // native-offset-keyed DeoptimizationPoints before the buffer is consumed.
    // Emit-and-discard: nothing reads these yet, so codegen is unchanged.
    let deopt_points = lowerer.build_deopt_points();
    let deopt_boxes = std::mem::take(&mut lowerer.deopt_boxes);

    let buf = lowerer.buf;
    let _code_size = buf.pos();

    let mut cm = CompiledMethod::new(buf);
    cm.deopt_points = deopt_points;
    cm._deopt_point_boxes = deopt_boxes;
    Some(cm)
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::IrBuilder;
    use crate::ir_optimize;
    use crate::ir_schedule;

    fn compile_via_ir(
        code: &[u8],
        code_len: usize,
        num_params: usize,
        num_locals: usize,
    ) -> Option<CompiledMethod> {
        let builder = IrBuilder::new(num_params, num_locals);
        let mut graph = builder.build(code, code_len)?;
        ir_optimize::optimize(&mut graph);
        let schedule = ir_schedule::schedule(&graph);
        lower(&graph, &schedule, num_params, num_locals)
    }

    /// Build → schedule → lower WITHOUT the optimizer, so safepoint NodeIds
    /// stay stable (DCE/GVN safepoint preservation is a later concern; the
    /// emit-and-discard points are validated on the un-optimized graph).
    fn compile_via_ir_no_opt(
        code: &[u8],
        code_len: usize,
        num_params: usize,
        num_locals: usize,
    ) -> CompiledMethod {
        let builder = IrBuilder::new(num_params, num_locals);
        let graph = builder.build(code, code_len).expect("IR build");
        let schedule = ir_schedule::schedule(&graph);
        lower(&graph, &schedule, num_params, num_locals).expect("lower")
    }

    // ── real-frame-deopt step 2: deopt points + lookup ───────────────────

    #[test]
    fn test_deopt_points_resolve_consts_and_slots() {
        use crate::deopt::FrameValue;
        // iconst_5; iconst_3; iadd; iconst_2; imul; ireturn
        //   pc0       pc1      pc2    pc3       pc4    pc5
        // Only the Add (pc2) and Mul (pc4) are data nodes carrying a
        // bytecode_pc, so exactly those two bcis anchor a deopt point.
        let code = [0x08, 0x06, 0x60, 0x05, 0x68, 0xac, 0, 0];
        let cm = compile_via_ir_no_opt(&code, 6, 0, 0);

        assert_eq!(cm.deopt_points.len(), 2, "deopt points for bci 2 and 4");
        // Sorted ascending by native_offset (emission order pc2 < pc4).
        assert!(cm.deopt_points[0].native_offset <= cm.deopt_points[1].native_offset);

        let p2 = cm.deopt_points.iter().find(|p| p.bci == 2).unwrap();
        // Before iadd: the two constants 5 and 3 are on the stack, encoded
        // directly (no machine location needed).
        assert_eq!(
            p2.frame_state.stack,
            vec![FrameValue::Int(5), FrameValue::Int(3)],
        );

        let p4 = cm.deopt_points.iter().find(|p| p.bci == 4).unwrap();
        // Before imul: [Add result, const 2]. The Add lives in a spill slot;
        // the constant is encoded directly.
        assert_eq!(p4.frame_state.stack.len(), 2);
        assert!(matches!(p4.frame_state.stack[0], FrameValue::StackSlot(off) if off < 0));
        assert_eq!(p4.frame_state.stack[1], FrameValue::Int(2));

        // Lookup roundtrip: a real native offset hits, a bogus one misses.
        let off = p2.native_offset;
        assert!(cm.find_deopt_point(off).is_some());
        assert_eq!(cm.find_deopt_point(off).unwrap().bci, 2);
        assert!(cm.find_deopt_point(off + 9999).is_none());
    }

    // ── real-frame-deopt step 3: route one guard end-to-end ──────────────

    #[test]
    fn test_guard_deopt_reconstructs_live_frame() {
        use crate::deopt::{take_last_deopt, FrameValue};
        use crate::ir::SafepointSnapshot;

        // Hand-build: i64 f(i64 cond, i64 val) {
        //     guard(cond != 0) [bci 5];   // deopt if cond == 0
        //     return val;
        // }
        // Node layout mirrors IrBuilder::new.
        let mut graph = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
        };
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let _mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let cond = graph.add(Op::Param(0), IrType::Int, vec![start], None);
        let val = graph.add(Op::Param(1), IrType::Int, vec![start], None);
        let _guard = graph.add(Op::Guard { bci: 5 }, IrType::Void, vec![ctrl, cond], None);
        let ret = graph.add(Op::Return, IrType::Void, vec![ctrl, val], None);
        graph.exit = ret;

        // Safepoint at the guard's bci: locals = [cond, val], empty stack.
        graph.safepoints.push(SafepointSnapshot {
            bci: 5,
            locals: vec![cond, val],
            stack: vec![],
        });

        let schedule = ir_schedule::schedule(&graph);
        let method = lower(&graph, &schedule, 2, 2).expect("lower guarded method");

        // Guard passes (cond != 0): normal return of `val`.
        let _ = take_last_deopt(); // clear any stale state
        let ok = unsafe { method.try_call(&[1, 777]).expect("call (guard ok)") };
        assert_eq!(ok, 777, "guard passes → returns val");
        assert!(take_last_deopt().is_none(), "no deopt when guard passes");

        // Guard fails (cond == 0): deopt sentinel returned, frame reconstructed
        // from the LIVE machine frame — locals resolved to the actual argument
        // values sitting in their spill slots.
        let sentinel = unsafe { method.try_call(&[0, 777]).expect("call (guard fail)") };
        assert_eq!(sentinel, i64::MIN, "guard fails → deopt sentinel");
        let frame = take_last_deopt().expect("deopt reconstructed a frame");
        assert_eq!(frame.bci, 5, "resumes at the guard's bci");
        assert_eq!(
            frame.locals,
            vec![FrameValue::Int(0), FrameValue::Int(777)],
            "locals read back from the live native frame",
        );
        assert!(frame.stack.is_empty());
        assert!(frame.caller_frames.is_empty());
    }

    #[test]
    fn test_deopt_points_params_use_slots() {
        use crate::deopt::FrameValue;
        // int f(int a, int b) { return a + b; } — iload_0;iload_1;iadd;ireturn
        let code = [0x1a, 0x1b, 0x60, 0xac, 0, 0];
        let cm = compile_via_ir_no_opt(&code, 4, 2, 2);
        // The Add (pc2) is the only bci with a data node → one deopt point.
        let p = cm.deopt_points.iter().find(|p| p.bci == 2).unwrap();
        // locals = [a, b], both live params → frame slots (negative off).
        assert_eq!(p.frame_state.locals.len(), 2);
        for v in &p.frame_state.locals {
            assert!(matches!(v, FrameValue::StackSlot(off) if *off < 0));
        }
        // stack at pc2 = [a, b] (same param values).
        assert_eq!(p.frame_state.stack.len(), 2);
    }

    #[test]
    fn test_lower_produces_code() {
        // int f(int x) { return x; }
        let code = [0x1a, 0xac, 0, 0];
        let compiled = compile_via_ir(&code, 2, 1, 1);
        assert!(compiled.is_some(), "Should produce compiled code");
        let method = compiled.unwrap();
        assert!(!method.entry_ptr().is_null(), "Code should not be empty");
    }

    #[test]
    fn test_lower_constant_return() {
        // int f() { return 42; }
        // bipush 42; ireturn
        let code = [0x10, 42, 0xac, 0, 0];
        let compiled = compile_via_ir(&code, 3, 0, 0);
        assert!(compiled.is_some());
        let method = compiled.unwrap();
        // Execute the compiled code
        let result = unsafe { method.try_call(&[]).expect("test JIT call") };
        assert_eq!(result, 42, "Should return 42");
    }

    #[test]
    fn test_lower_add_constants() {
        // int f() { return 3 + 4; }  → constant-folded to return 7
        let code = [0x06, 0x07, 0x60, 0xac, 0, 0];
        let compiled = compile_via_ir(&code, 4, 0, 0);
        assert!(compiled.is_some());
        let method = compiled.unwrap();
        let result = unsafe { method.try_call(&[]).expect("test JIT call") };
        assert_eq!(result, 7, "Should return 7 after constant folding");
    }

    #[test]
    fn test_lower_add_params() {
        // int f(int a, int b) { return a + b; }
        let code = [0x1a, 0x1b, 0x60, 0xac, 0, 0];
        let compiled = compile_via_ir(&code, 4, 2, 2);
        assert!(compiled.is_some());
        let method = compiled.unwrap();
        let result = unsafe { method.try_call(&[10, 20]).expect("test JIT call") };
        assert_eq!(result, 30, "10 + 20 = 30");
    }

    #[test]
    fn test_lower_sub_params() {
        // int f(int a, int b) { return a - b; }
        let code = [0x1a, 0x1b, 0x64, 0xac, 0, 0];
        let compiled = compile_via_ir(&code, 4, 2, 2);
        assert!(compiled.is_some());
        let method = compiled.unwrap();
        let result = unsafe { method.try_call(&[30, 12]).expect("test JIT call") };
        assert_eq!(result, 18, "30 - 12 = 18");
    }

    #[test]
    fn test_lower_mul_params() {
        // int f(int a, int b) { return a * b; }
        let code = [0x1a, 0x1b, 0x68, 0xac, 0, 0];
        let compiled = compile_via_ir(&code, 4, 2, 2);
        assert!(compiled.is_some());
        let method = compiled.unwrap();
        let result = unsafe { method.try_call(&[6, 7]).expect("test JIT call") };
        assert_eq!(result, 42, "6 * 7 = 42");
    }

    // ── Regression: BUG FIX [jit-irlower #1 + #3] — Op::Cmp SETcc ────────
    //
    // Hand-build a single-block graph whose Return value is a bare
    // `Op::Cmp(Lt)`, so the SETcc store is the only thing producing the
    // result. Two bugs had to be fixed: #1 the opcode (`+ 0x10` → `0F 9C` =
    // SETL, not the old `- 0x10` MMX byte), and #3 the MISSING ModRM byte —
    // SETcc is a /digit form, so `0F 9C` without `0xC0` desynced the stream
    // (the next `0F B6` MOVZX was partly consumed as the ModRM) and AL/RAX
    // were never written, so the Cmp returned the first operand (`a`) instead
    // of the 0/1 boolean. With `0F 9C C0` the boolean is correct.
    #[test]
    fn test_lower_cmp_lt_setcc() {
        use crate::ir::{CmpOp, Graph, IrType, Op, NO_NODE};

        // Mirror IrBuilder::new node layout: Start, Proj(0)=ctrl, Proj(1)=mem,
        // Param(0), Param(1).
        let mut graph = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
        };
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let _mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let a = graph.add(Op::Param(0), IrType::Int, vec![start], None);
        let b = graph.add(Op::Param(1), IrType::Int, vec![start], None);
        // cmp = (a < b) ? 1 : 0
        let cmp = graph.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![a, b], None);
        // Return [ctrl, cmp]
        let ret = graph.add(Op::Return, IrType::Void, vec![ctrl, cmp], None);
        graph.exit = ret;

        let schedule = ir_schedule::schedule(&graph);
        let method = lower(&graph, &schedule, 2, 2).expect("lower cmp graph");

        // a < b  → 1
        let r_true = unsafe { method.try_call(&[3, 7]).expect("test JIT call") };
        assert_eq!(r_true, 1, "3 < 7 should set the boolean to 1");
        // a >= b → 0
        let r_false = unsafe { method.try_call(&[7, 3]).expect("test JIT call") };
        assert_eq!(r_false, 0, "7 < 3 is false → 0");
        let r_eq = unsafe { method.try_call(&[5, 5]).expect("test JIT call") };
        assert_eq!(r_eq, 0, "5 < 5 is false → 0");
    }

    // ── Regression: BUG FIX [jit-irlower #1 + #2] — ternary via IR path ──
    //
    // int f(int a, int b) { return a < b ? 1 : 0; }
    //
    // javac lowers the ternary to a compare-and-branch joined by a phi:
    //   iload_0; iload_1; if_icmpge else; iconst_1; goto end;
    //   else: iconst_0; end: ireturn
    // This exercises the fixes end-to-end: the `Op::Cmp(Ge)` feeding the `If`
    // (#1 + #3 SETcc) and the edge-split parallel copy that materialises the
    // phi value 1/0 at each branch edge (#2). The earlier "always resolves to
    // the else value" symptom was a downstream effect of the SETcc desync (#3):
    // the `If` branched on the first operand instead of the comparison, so it
    // always took the same edge. With the SETcc ModRM fix the phi/branch path
    // is correct.
    #[test]
    fn test_lower_ternary_lt_phi() {
        // PCs:
        //  0: iload_0      1a
        //  1: iload_1      1b
        //  2: if_icmpge 9  a2 00 07   (offset 7 from pc 2 → pc 9)
        //  5: iconst_1     04
        //  6: goto 10      a7 00 04   (offset 4 from pc 6 → pc 10)
        //  9: iconst_0     03
        // 10: ireturn      ac
        let code = [
            0x1a, 0x1b, 0xa2, 0x00, 0x07, 0x04, 0xa7, 0x00, 0x04, 0x03, 0xac, 0, 0,
        ];
        let compiled = compile_via_ir(&code, 11, 2, 2);
        assert!(compiled.is_some(), "ternary should compile via IR path");
        let method = compiled.unwrap();

        let r_true = unsafe { method.try_call(&[3, 7]).expect("test JIT call") };
        assert_eq!(r_true, 1, "3 < 7 → 1");
        let r_false = unsafe { method.try_call(&[7, 3]).expect("test JIT call") };
        assert_eq!(r_false, 0, "7 < 3 → 0");
        let r_eq = unsafe { method.try_call(&[5, 5]).expect("test JIT call") };
        assert_eq!(r_eq, 0, "5 == 5, not < → 0");
    }

    // ── Regression: the canonical production SIGSEGV shape — a pure,
    // call-free branchy predicate (e.g. `Modifier.isStatic`). Before the
    // SETcc ModRM fix (#3) this exact shape emitted a stray `SETcc [rdi]`
    // (write through a near-null base) → SIGSEGV, which is why
    // `jit/src/lib.rs` declined branchy call-free methods from the IR path.
    #[test]
    fn test_lower_and_predicate_isstatic_shape() {
        // static boolean f(int m) { return (m & 8) != 0; }
        //  0: iload_0     1a
        //  1: bipush 8    10 08
        //  3: iand        7e
        //  4: ifeq 11     99 00 07   (m&8 == 0 → pc 11)
        //  7: iconst_1    04
        //  8: goto 12     a7 00 04
        // 11: iconst_0    03
        // 12: ireturn     ac
        let code = [
            0x1a, 0x10, 0x08, 0x7e, 0x99, 0x00, 0x07, 0x04, 0xa7, 0x00, 0x04, 0x03, 0xac, 0, 0,
        ];
        let cm = compile_via_ir(&code, 13, 1, 1).expect("branchy predicate compiles via IR");
        let f = |m: i64| unsafe { cm.try_call(&[m]).expect("call") };
        assert_eq!(f(8), 1, "8 & 8 != 0 → true");
        assert_eq!(f(0), 0, "0 & 8 == 0 → false");
        assert_eq!(f(7), 0, "7 & 8 == 0 → false");
        assert_eq!(f(15), 1, "15 & 8 != 0 → true");
    }

    // ── Regression: a phi that merges non-constant operand values (param a
    // vs param b) across the two branch edges, not just 0/1 constants —
    // exercises the edge-split parallel copy with live values.
    #[test]
    fn test_lower_max_phi_merges_params() {
        // int max(int a, int b) { return a >= b ? a : b; }
        //  0: iload_0     1a
        //  1: iload_1     1b
        //  2: if_icmplt 9 a1 00 07   (a < b → pc 9, return b)
        //  5: iload_0     1a
        //  6: goto 10     a7 00 04
        //  9: iload_1     1b
        // 10: ireturn     ac
        let code = [
            0x1a, 0x1b, 0xa1, 0x00, 0x07, 0x1a, 0xa7, 0x00, 0x04, 0x1b, 0xac, 0, 0,
        ];
        let cm = compile_via_ir(&code, 11, 2, 2).expect("max compiles via IR");
        let max = |a: i64, b: i64| unsafe { cm.try_call(&[a, b]).expect("call") };
        assert_eq!(max(3, 7), 7, "max(3,7)=7");
        assert_eq!(max(7, 3), 7, "max(7,3)=7");
        assert_eq!(max(5, 5), 5, "max(5,5)=5");
        assert_eq!(max(-2, -9), -2, "max(-2,-9)=-2");
    }

    // ── Gate-flip readiness probe: a COUNTED LOOP (loop-carried phi over a
    // back-edge). If this fails, the IR builder/lowerer does not yet handle
    // loop phis, and branchy call-free methods (which include loops) must NOT
    // be routed to the IR path — i.e. the lib.rs gate cannot be flipped on the
    // SETcc fix alone.
    #[test]
    #[ignore = "GATE-FLIP BLOCKER: the IR builder is a single forward pass and \
                does not fold loop back-edge values into the loop-header phi, so \
                a counted loop's loop-carried accumulator is lost (sum returns \
                the initial 0). The SETcc fix (#3) unblocks if/else branches, \
                but branchy call-free methods include LOOPS, so jit/src/lib.rs \
                must keep declining them from the IR path until loop-phi support \
                lands. This test is the readiness guard for that work."]
    fn test_lower_counted_loop_sum() {
        // int sum(int n){ int s=0; for(int i=0;i<n;i++) s+=i; return s; }
        //  0: iconst_0  1: istore_1  2: iconst_0  3: istore_2
        //  4: iload_2   5: iload_0   6: if_icmpge 19
        //  9: iload_1  10: iload_2  11: iadd  12: istore_1
        // 13: iinc 2,1 16: goto 4   19: iload_1 20: ireturn
        let code = [
            0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x0d, 0x1b, 0x1c, 0x60, 0x3c, 0x84,
            0x02, 0x01, 0xa7, 0xff, 0xf4, 0x1b, 0xac, 0, 0,
        ];
        let cm = compile_via_ir(&code, 21, 1, 3).expect("loop compiles via IR");
        let sum = |n: i64| unsafe { cm.try_call(&[n]).expect("call") };
        assert_eq!(sum(5), 10, "0+1+2+3+4 = 10");
        assert_eq!(sum(0), 0, "empty loop = 0");
        assert_eq!(sum(10), 45, "sum 0..9 = 45");
    }
}
