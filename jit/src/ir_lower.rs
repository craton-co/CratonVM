// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lower the scheduled IR graph to x86-64 machine code.
//!
//! Walks the scheduled basic blocks, emits native instructions for each
//! IR node, and patches forward branches.

use std::collections::HashMap;

use super::ir::{Graph, IrType, NodeId, Op, SafepointSnapshot, NO_NODE};
use super::ir_schedule::Schedule;
use super::{CompiledMethod, ExecutableBuffer, JitRuntimeHelpers};
use crate::deopt::{
    ir_deopt_entry, DeoptAction, DeoptReason, DeoptimizationPoint, FrameState, FrameValue,
};
use cratonvm_types::{FIELD_CELL_PAYLOAD32_OFFSET, HEADER_SIZE, SLOT_SIZE};

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
#[allow(dead_code)]
const R10: u8 = 10;

// XMM scratch registers for the FP value tier (inc 30). Analogous to RAX/RCX:
// XMM0 holds the first operand / result, XMM1 the second operand / a mask.
const XMM0: u8 = 0;
const XMM1: u8 = 1;

// Platform C-ABI integer argument registers for the `invoke_dispatch` helper
// call (Gap B `Op::Call`). The helper's 4 args (vm_ptr, info_ptr, args_ptr,
// num_args) are all integer, so all four fit in registers on both ABIs — no
// stack args, only the 32-byte Win64 shadow space (already budgeted in the
// frame). Mirrors x64.rs `ARG_REGS`.
#[cfg(target_os = "windows")]
const CALL_ARG_REGS: [u8; 4] = [1, 2, 8, 9]; // RCX, RDX, R8, R9
#[cfg(not(target_os = "windows"))]
const CALL_ARG_REGS: [u8; 4] = [7, 6, 2, 1]; // RDI, RSI, RDX, RCX

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
    // ── Gap B: Op::Call (invokestatic via the dispatch helper) ───────────
    /// Address of the `jit_invoke_dispatch` runtime helper (baked into each
    /// `Op::Call` site as `MOV RAX,imm64 ; CALL RAX`). 0 if no calls.
    invoke_dispatch: usize,
    /// True iff the graph contains an `Op::Call` — then the method takes the VM
    /// context pointer as a hidden first argument (`try_call_with_context`), and
    /// the prologue stores it to `context_slot_off` + shifts the Java params.
    needs_context: bool,
    /// Frame offset of the saved VM context pointer (valid iff `needs_context`).
    context_slot_off: i32,
    /// Frame offset of `arg[0]` in the Java-argument staging region a call
    /// marshals its args into; `arg[i]` lives at `args_stage_top_off - i*8`
    /// (increasing address), and `args_ptr = rbp - args_stage_top_off`.
    args_stage_top_off: i32,
    /// Upper bound (inclusive) for a spill slot's frame offset — excludes the
    /// shadow space AND the arg-staging region so spills never overlap them.
    spill_cap_off: i32,
    /// Native offsets of `JE rel32` instructions emitted after each dispatch
    /// call (the exception sentinel check) that jump to the shared bail stub.
    call_exc_patches: Vec<usize>,
}

impl<'a> Lowerer<'a> {
    fn new(
        graph: &'a Graph,
        schedule: &'a Schedule,
        buf: ExecutableBuffer,
        num_params: usize,
        num_locals: usize,
        max_nodes: usize,
        helpers: &JitRuntimeHelpers,
    ) -> Self {
        // Gap B: scan for `Op::Call` to size the call-related frame regions.
        // `needs_context` ⇒ the method takes the VM ptr as a hidden first arg
        // and reserves a context slot. `max_call_args` sizes the Java-argument
        // staging region a call marshals its args into before dispatching.
        let mut needs_context = false;
        let mut max_call_args = 0usize;
        for n in &graph.nodes {
            if matches!(n.op, Op::Call { .. }) {
                needs_context = true;
                // inputs = [ctrl, mem, args…]
                max_call_args = max_call_args.max(n.inputs.len().saturating_sub(2));
            }
        }

        // Frame layout (rbp downward): locals, [context slot], spills, [args
        // staging], 16-byte stack-arg reserve, 32-byte shadow. Reserve slots for
        // locals + max_nodes spills + shadow. The 16-byte tail above the shadow
        // region holds in-frame stack args for any helper called without
        // `emit_stack_arg_setup`; see the matching comment in `x64.rs`
        // (Compiler::new) for the worst-case 6-arg `jit_invoke_virtual_mic` site.
        let locals_size = (num_locals as i32) * 8;
        let context_size = if needs_context { 8 } else { 0 };
        let spill_size = (max_nodes as i32) * 8;
        let args_stage_size = (max_call_args as i32) * 8;
        let shadow = 32i32;
        let stack_arg_reserve = 16i32;
        let total =
            locals_size + context_size + spill_size + args_stage_size + shadow + stack_arg_reserve;
        let frame_size = (total + 15) & !15;

        // The context slot is the first slot after the locals; spills start
        // after it. `arg[0]` of the staging region sits at offset
        // `frame_size - shadow` (the byte just above the shadow space); for a
        // no-call method `args_stage_size == 0` so the cap stays `frame_size -
        // shadow`, unchanged from before this slice.
        let context_slot_off = if needs_context {
            (num_locals as i32 + 1) * 8
        } else {
            0
        };
        let first_spill = (num_locals as i32 + 1 + if needs_context { 1 } else { 0 }) * 8;
        let args_stage_top_off = frame_size - shadow;
        let spill_cap_off = frame_size - shadow - args_stage_size;

        Lowerer {
            graph,
            schedule,
            buf,
            node_slot: vec![0; graph.nodes.len()],
            next_spill: first_spill,
            block_offsets: vec![0; schedule.blocks.len()],
            branch_patches: Vec::new(),
            num_params,
            _num_locals: num_locals,
            frame_size,
            bci_native: HashMap::new(),
            deopt_stub_patches: Vec::new(),
            deopt_boxes: Vec::new(),
            invoke_dispatch: helpers.invoke_dispatch,
            needs_context,
            context_slot_off,
            args_stage_top_off,
            spill_cap_off,
            call_exc_patches: Vec::new(),
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
        // `spill_cap_off` excludes the 32-byte shadow space AND (Gap B) the
        // Java-arg staging region, so a spill never overlaps either. For a
        // no-call method it equals `frame_size - DEOPT_SHADOW_SPACE` — the
        // historical bound, unchanged.
        assert!(
            offset <= self.spill_cap_off,
            "JIT lowerer: spill offset {} exceeds frame capacity {} \
             (cap {}, less shadow + arg-staging reserve)",
            offset,
            self.frame_size,
            self.spill_cap_off,
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
        // Windows: RCX, RDX, R8, R9.  SysV: RDI, RSI, RDX, RCX, R8, R9.
        #[cfg(target_os = "windows")]
        let abi_regs: &[u8] = &[RCX, RDX, 8, 9]; // RCX, RDX, R8, R9
        #[cfg(not(target_os = "windows"))]
        let abi_regs: &[u8] = &[7, 6, RDX, RCX, 8, 9]; // RDI, RSI, RDX, RCX, R8, R9

        // Gap B: a `needs_context` method receives the VM context pointer in
        // ABI[0] (the `try_call_with_context` convention), with the Java params
        // shifted to ABI[1..]. Store the context to its slot, then the params to
        // their local slots. `lower()` bails (single-pass) before reaching here
        // if `1 + num_params` would exceed the register args, so every param
        // below comes from a register.
        let base = if self.needs_context {
            self.store_abi_reg(abi_regs[0], self.context_slot_off);
            1
        } else {
            0
        };
        for i in 0..self.num_params {
            let abi_idx = base + i;
            if abi_idx >= abi_regs.len() {
                break;
            }
            self.store_abi_reg(abi_regs[abi_idx], ((i as i32) + 1) * 8); // local_offset(i)
        }
    }

    /// MOV [RBP - offset], reg  (REX.W [+ REX.R for an extended reg]).
    /// Prefers the disp8 ModRM form when `-offset` fits in a signed byte.
    fn store_abi_reg(&mut self, reg: u8, offset: i32) {
        let neg = -offset;
        let mut prefix = 0x48u8; // REX.W
        if reg >= 8 {
            prefix |= 0x04; // REX.R
        }
        self.buf.emit_byte(prefix);
        self.buf.emit_byte(0x89);
        if (i8::MIN as i32..=i8::MAX as i32).contains(&neg) {
            self.buf.emit_byte(0x45 | ((reg & 7) << 3));
            self.buf.emit_byte(neg as u8);
        } else {
            self.buf.emit_byte(0x85 | ((reg & 7) << 3));
            self.buf.emit(&neg.to_le_bytes());
        }
    }

    /// MOV reg, [RBP - offset]  (REX.W [+ REX.R]; disp32 form). General form of
    /// `load_to_rax`/`load_to_rcx` for an arbitrary (possibly extended) dest.
    fn load_reg_from_frame(&mut self, reg: u8, offset: i32) {
        let neg = -offset;
        let mut prefix = 0x48u8;
        if reg >= 8 {
            prefix |= 0x04;
        }
        self.buf.emit_byte(prefix);
        self.buf.emit_byte(0x8B);
        self.buf.emit_byte(0x85 | ((reg & 7) << 3));
        self.buf.emit(&neg.to_le_bytes());
    }

    /// LEA reg, [RBP - offset]  (REX.W [+ REX.R]; disp32 form). Used to compute
    /// the `args_ptr` the dispatch helper reads the marshalled Java args from.
    fn lea_reg_from_frame(&mut self, reg: u8, offset: i32) {
        let neg = -offset;
        let mut prefix = 0x48u8;
        if reg >= 8 {
            prefix |= 0x04;
        }
        self.buf.emit_byte(prefix);
        self.buf.emit_byte(0x8D);
        self.buf.emit_byte(0x85 | ((reg & 7) << 3));
        self.buf.emit(&neg.to_le_bytes());
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

    // ── FP value tier (inc 30) — XMM scratch helpers ─────────────────────
    //
    // The naive spill-everything model extends cleanly to FP: every value
    // (int/long/ref AND float/double) lives in a frame slot as raw bits. FP
    // arithmetic and conversions load operand bits from slots into XMM0/XMM1
    // (scratch — caller-saved on both ABIs, and no value lives across nodes),
    // compute, and store the result back. Constants and FP negation never touch
    // XMM: a float/double constant is just its bit pattern written via a GPR
    // immediate, and negation is a sign-bit XOR on the integer bit pattern.
    // XMM0/XMM1 are the FP analogues of RAX/RCX. Both are < 8, so no REX is
    // needed; `[rbp - offset]` always uses the disp32 ModRM form (mod=10, the
    // `0x85 | reg<<3` byte) for simplicity.

    /// MOVSS/MOVSD xmm, [rbp - offset] — load a 32/64-bit FP value from a slot.
    fn fp_load(&mut self, xmm: u8, offset: i32, is_double: bool) {
        let neg = -offset;
        self.buf.emit_byte(if is_double { 0xF2 } else { 0xF3 });
        self.buf.emit(&[0x0F, 0x10]);
        self.buf.emit_byte(0x85 | ((xmm & 7) << 3));
        self.buf.emit(&neg.to_le_bytes());
    }

    /// MOVSS/MOVSD [rbp - offset], xmm — store an FP value to a slot. A `MOVSS`
    /// writes only the low 4 bytes; the slot's high 4 are left stale, which is
    /// harmless because every float consumer reads it back with `MOVSS` (4 bytes).
    fn fp_store(&mut self, offset: i32, xmm: u8, is_double: bool) {
        let neg = -offset;
        self.buf.emit_byte(if is_double { 0xF2 } else { 0xF3 });
        self.buf.emit(&[0x0F, 0x11]);
        self.buf.emit_byte(0x85 | ((xmm & 7) << 3));
        self.buf.emit(&neg.to_le_bytes());
    }

    /// Scalar FP binary op (`<prefix> 0F <op>`), reg-reg form `dst op= src`.
    /// `op` is the second opcode byte: ADD=0x58, SUB=0x5C, MUL=0x59, DIV=0x5E.
    fn fp_binop(&mut self, op: u8, dst: u8, src: u8, is_double: bool) {
        self.buf.emit_byte(if is_double { 0xF2 } else { 0xF3 });
        self.buf.emit(&[0x0F, op]);
        self.buf.emit_byte(0xC0 | ((dst & 7) << 3) | (src & 7));
    }

    /// IEEE-754 NaN/overflow fixup after a `CVTTSS2SI`/`CVTTSD2SI` whose source
    /// is still in XMM0 and whose (sentinel-or-real) result is in EAX/RAX.
    ///
    /// x86 `CVTT*` yields the "integer indefinite" (0x8000_0000 / 0x8000…0) for
    /// NaN AND any out-of-range/∞ input, but the JVM requires NaN→0,
    /// +overflow→MAX, −overflow→MIN. This ports the single-pass backend's
    /// `emit_fp_to_int_nan_fixup` verbatim so the two backends agree bit-for-bit.
    /// Uses XMM1 as scratch (PXOR to materialize +0.0 for the sign test).
    fn emit_fp_to_int_fixup(&mut self, is_double: bool, is_long: bool) {
        if !is_long {
            // CMP EAX, 0x80000000
            self.buf.emit_byte(0x3D);
            self.buf.emit(&0x80000000u32.to_le_bytes());
            // JNE .done
            self.buf.emit_byte(0x75);
            let jne_patch = self.buf.pos();
            self.buf.emit_byte(0x00);
            // UCOMISD/UCOMISS XMM0, XMM0 — PF=1 if NaN
            if is_double {
                self.buf.emit(&[0x66, 0x0F, 0x2E, 0xC0]);
            } else {
                self.buf.emit(&[0x0F, 0x2E, 0xC0]);
            }
            // JP .nan
            self.buf.emit_byte(0x7A);
            let jp_patch = self.buf.pos();
            self.buf.emit_byte(0x00);
            // Not NaN — overflow. PXOR XMM1,XMM1 then compare sign.
            self.buf.emit(&[0x66, 0x0F, 0xEF, 0xC9]);
            if is_double {
                self.buf.emit(&[0x66, 0x0F, 0x2E, 0xC1]);
            } else {
                self.buf.emit(&[0x0F, 0x2E, 0xC1]);
            }
            // JBE .done (negative overflow — 0x80000000 already correct)
            self.buf.emit_byte(0x76);
            let jbe_patch = self.buf.pos();
            self.buf.emit_byte(0x00);
            // Positive overflow: MOV EAX, 0x7FFFFFFF ; JMP .done
            self.buf.emit_byte(0xB8);
            self.buf.emit(&0x7FFFFFFFu32.to_le_bytes());
            self.buf.emit_byte(0xEB);
            let jmp_patch = self.buf.pos();
            self.buf.emit_byte(0x00);
            // .nan: XOR EAX,EAX
            let nan_off = self.buf.pos();
            self.buf
                .try_patch_byte(jp_patch, (nan_off - jp_patch - 1) as u8)
                .ok();
            self.buf.emit(&[0x31, 0xC0]);
            // .done:
            let done_off = self.buf.pos();
            self.buf
                .try_patch_byte(jne_patch, (done_off - jne_patch - 1) as u8)
                .ok();
            self.buf
                .try_patch_byte(jbe_patch, (done_off - jbe_patch - 1) as u8)
                .ok();
            self.buf
                .try_patch_byte(jmp_patch, (done_off - jmp_patch - 1) as u8)
                .ok();
        } else {
            // MOV RCX, 0x8000000000000000 ; CMP RAX, RCX
            self.buf.emit(&[0x48, 0xB9]);
            self.buf.emit(&0x8000000000000000u64.to_le_bytes());
            self.buf.emit(&[0x48, 0x39, 0xC8]);
            // JNE .done
            self.buf.emit_byte(0x75);
            let jne_patch = self.buf.pos();
            self.buf.emit_byte(0x00);
            // UCOMI XMM0,XMM0 ; JP .nan
            if is_double {
                self.buf.emit(&[0x66, 0x0F, 0x2E, 0xC0]);
            } else {
                self.buf.emit(&[0x0F, 0x2E, 0xC0]);
            }
            self.buf.emit_byte(0x7A);
            let jp_patch = self.buf.pos();
            self.buf.emit_byte(0x00);
            // PXOR XMM1,XMM1 ; UCOMI XMM0,XMM1
            self.buf.emit(&[0x66, 0x0F, 0xEF, 0xC9]);
            if is_double {
                self.buf.emit(&[0x66, 0x0F, 0x2E, 0xC1]);
            } else {
                self.buf.emit(&[0x0F, 0x2E, 0xC1]);
            }
            // JBE .done (negative overflow)
            self.buf.emit_byte(0x76);
            let jbe_patch = self.buf.pos();
            self.buf.emit_byte(0x00);
            // Positive overflow: MOV RAX, 0x7FFFFFFFFFFFFFFF ; JMP .done
            self.buf.emit(&[0x48, 0xB8]);
            self.buf.emit(&0x7FFFFFFFFFFFFFFFu64.to_le_bytes());
            self.buf.emit_byte(0xEB);
            let jmp_patch = self.buf.pos();
            self.buf.emit_byte(0x00);
            // .nan: XOR RAX,RAX
            let nan_off = self.buf.pos();
            self.buf
                .try_patch_byte(jp_patch, (nan_off - jp_patch - 1) as u8)
                .ok();
            self.buf.emit(&[0x48, 0x31, 0xC0]);
            // .done:
            let done_off = self.buf.pos();
            self.buf
                .try_patch_byte(jne_patch, (done_off - jne_patch - 1) as u8)
                .ok();
            self.buf
                .try_patch_byte(jbe_patch, (done_off - jbe_patch - 1) as u8)
                .ok();
            self.buf
                .try_patch_byte(jmp_patch, (done_off - jmp_patch - 1) as u8)
                .ok();
        }
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
                if matches!(node.ty, IrType::Float | IrType::Double) {
                    // ADDSS/ADDSD XMM0, XMM1
                    let is_d = node.ty == IrType::Double;
                    self.fp_load(XMM0, self.slot_of(node.inputs[0]), is_d);
                    self.fp_load(XMM1, self.slot_of(node.inputs[1]), is_d);
                    self.fp_binop(0x58, XMM0, XMM1, is_d);
                    self.fp_store(slot, XMM0, is_d);
                } else {
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
            }
            Op::Sub => {
                let slot = self.alloc_slot(id);
                if matches!(node.ty, IrType::Float | IrType::Double) {
                    // SUBSS/SUBSD XMM0, XMM1
                    let is_d = node.ty == IrType::Double;
                    self.fp_load(XMM0, self.slot_of(node.inputs[0]), is_d);
                    self.fp_load(XMM1, self.slot_of(node.inputs[1]), is_d);
                    self.fp_binop(0x5C, XMM0, XMM1, is_d);
                    self.fp_store(slot, XMM0, is_d);
                } else {
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
            }
            Op::Mul => {
                let slot = self.alloc_slot(id);
                if matches!(node.ty, IrType::Float | IrType::Double) {
                    // MULSS/MULSD XMM0, XMM1
                    let is_d = node.ty == IrType::Double;
                    self.fp_load(XMM0, self.slot_of(node.inputs[0]), is_d);
                    self.fp_load(XMM1, self.slot_of(node.inputs[1]), is_d);
                    self.fp_binop(0x59, XMM0, XMM1, is_d);
                    self.fp_store(slot, XMM0, is_d);
                } else {
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
            }
            Op::Div => {
                let slot = self.alloc_slot(id);
                // FP division has NO zero/overflow guard: IEEE x/0 is ±inf/NaN,
                // never an exception, so an `fdiv`/`ddiv` is a plain DIVSS/DIVSD
                // and is never a deopt point.
                if matches!(node.ty, IrType::Float | IrType::Double) {
                    let is_d = node.ty == IrType::Double;
                    self.fp_load(XMM0, self.slot_of(node.inputs[0]), is_d);
                    self.fp_load(XMM1, self.slot_of(node.inputs[1]), is_d);
                    self.fp_binop(0x5E, XMM0, XMM1, is_d);
                    self.fp_store(slot, XMM0, is_d);
                    return;
                }
                let ty = node.ty;
                let bpc = node.bytecode_pc;
                self.load_to_rax(self.slot_of(node.inputs[0]));
                self.load_to_rcx(self.slot_of(node.inputs[1]));
                self.emit_div_zero_guard(ty, bpc);
                // JVMS MIN/-1 overflow guard: materialise MIN and skip the IDIV
                // (a raw IDIV on MIN/-1 raises #DE).
                let ovf_after = self.emit_div_overflow_guard(ty, /* is_rem */ false);
                if ty == IrType::Int {
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
                self.patch_div_overflow_after(ovf_after);
                self.store_rax(slot);
            }
            Op::Rem => {
                let slot = self.alloc_slot(id);
                let ty = node.ty;
                let bpc = node.bytecode_pc;
                self.load_to_rax(self.slot_of(node.inputs[0]));
                self.load_to_rcx(self.slot_of(node.inputs[1]));
                self.emit_div_zero_guard(ty, bpc);
                // JVMS MIN/-1 overflow guard: materialise remainder 0 and skip
                // the IDIV (a raw IDIV on MIN/-1 raises #DE).
                let ovf_after = self.emit_div_overflow_guard(ty, /* is_rem */ true);
                if ty == IrType::Int {
                    self.buf.emit_byte(0x99); // CDQ
                    self.buf.emit(&[0xF7, 0xF9]); // IDIV ECX
                } else {
                    self.buf.emit(&[0x48, 0x99]); // CQO
                    self.buf.emit(&[0x48, 0xF7, 0xF9]); // IDIV RCX
                }
                // Remainder is in RDX; move to RAX
                // MOV RAX, RDX
                self.buf.emit(&[0x48, 0x89, 0xD0]);
                self.patch_div_overflow_after(ovf_after);
                self.store_rax(slot);
            }
            Op::Neg => {
                let slot = self.alloc_slot(id);
                self.load_to_rax(self.slot_of(node.inputs[0]));
                match node.ty {
                    // FP negation = flip the IEEE sign bit of the bit pattern
                    // (correct for ±0.0 and NaN, unlike `0.0 - x`). Done on the
                    // integer pattern in RAX — no XMM needed. The float result's
                    // high 4 slot bytes are left zero; every consumer reads it
                    // back with MOVSS (4 bytes).
                    IrType::Float => {
                        // XOR EAX, 0x80000000
                        self.buf.emit_byte(0x35);
                        self.buf.emit(&0x80000000u32.to_le_bytes());
                    }
                    IrType::Double => {
                        // MOV RCX, 0x8000000000000000 ; XOR RAX, RCX
                        self.emit_mov_reg_imm64(RCX, 0x8000000000000000u64);
                        self.buf.emit(&[0x48, 0x31, 0xC8]);
                    }
                    IrType::Int => {
                        // NEG EAX
                        self.buf.emit(&[0xF7, 0xD8]);
                    }
                    _ => {
                        // NEG RAX (Long)
                        self.buf.emit(&[0x48, 0xF7, 0xD8]);
                    }
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
            // inc 27: `lcmp` 3-way signed compare of two longs → int {-1,0,1}.
            // result = (a > b) − (a < b), using signed SETcc on a 64-bit CMP, then
            // sign-extended to 64 bits so a 32- or 64-bit consumer both read it
            // correctly (the typical consumer is an `if<cond>` against 0).
            Op::LCmp => {
                let slot = self.alloc_slot(id);
                self.load_to_rax(self.slot_of(node.inputs[0])); // a
                self.load_to_rcx(self.slot_of(node.inputs[1])); // b
                self.buf.emit(&[0x48, 0x39, 0xC8]); // CMP RAX, RCX (signed, 64-bit)
                self.buf.emit(&[0x0F, 0x9F, 0xC0]); // SETG AL  (a > b)
                self.buf.emit(&[0x0F, 0x9C, 0xC2]); // SETL DL  (a < b)
                self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                self.buf.emit(&[0x0F, 0xB6, 0xD2]); // MOVZX EDX, DL
                self.buf.emit(&[0x29, 0xD0]); // SUB EAX, EDX  → {-1,0,1}
                self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX (sign-extend)
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
            // getfield read — `Op::Load`. The IR builder emits only
            // `Op::Load(MemKind::Int)` (int-category instance fields, slice 1
            // of the field/call frontier), so this lowers the single-pass
            // inline-getfield ABI exactly: null receiver → 0, else MOVSXD the
            // 32-bit `Value::Int` payload. inputs = [ctrl, mem, base, offset]
            // where `offset` is a `Const(field_index)`.
            Op::Load(_) => {
                let slot = self.alloc_slot(id);
                let base = node.inputs[2];
                let offset_node = node.inputs[3];
                let field_index = match self.graph.nodes[offset_node as usize].op {
                    Op::Const(v) => v,
                    _ => 0,
                };
                // Byte displacement of the field's 32-bit Int payload within
                // the object: HEADER_SIZE + field_index*SLOT_SIZE +
                // FIELD_CELL_PAYLOAD32_OFFSET (the same arithmetic the
                // single-pass inline getfield uses).
                let disp = HEADER_SIZE as i32
                    + (field_index as i32) * SLOT_SIZE as i32
                    + FIELD_CELL_PAYLOAD32_OFFSET as i32;
                // Receiver pointer → RAX (64-bit; a Param slot holds the full
                // pointer the prologue stored from the argument register).
                self.load_to_rax(self.slot_of(base));
                // TEST RAX,RAX ; JE +9 → null path (the trailing XOR EAX,EAX).
                self.buf.emit(&[0x48, 0x85, 0xC0]);
                self.buf.emit(&[0x74, 0x09]);
                // MOVSXD RAX, [RAX + disp32]  (sign-extend the Int payload).
                self.buf.emit(&[0x48, 0x63, 0x80]);
                self.buf.emit(&disp.to_le_bytes());
                // JMP +2 → done (skip the null path).
                self.buf.emit(&[0xEB, 0x02]);
                // null path: RAX := 0, matching `jit_getfield`'s null guard.
                self.buf.emit(&[0x31, 0xC0]);
                // done: spill the result.
                self.store_rax(slot);
            }
            // putfield write — `Op::Store`. The IR builder emits only
            // `Op::Store(MemKind::Int)` (int-category instance fields). Inline
            // the `jit_putfield_int` heap write: null receiver → no-op, else
            // write a `Value::Int(value)` cell (discriminant 0 + the 32-bit
            // payload, high qword cleared so no stale ref/garbage survives —
            // mirroring the scalar-replace store and the real helper).
            // inputs = [ctrl, mem, base, offset, value]; produces no value
            // (a pure memory-ordering token), so no slot is allocated.
            Op::Store(_) => {
                let base = node.inputs[2];
                let offset_node = node.inputs[3];
                let value = node.inputs[4];
                let field_index = match self.graph.nodes[offset_node as usize].op {
                    Op::Const(v) => v,
                    _ => 0,
                };
                let tag_off = HEADER_SIZE as i32 + (field_index as i32) * SLOT_SIZE as i32;
                let pay_off = tag_off + FIELD_CELL_PAYLOAD32_OFFSET as i32;
                let high_off = tag_off + 8; // the 8-byte payload region (Long/ref)
                                            // Receiver → RAX, value → RCX. Both loaded BEFORE the null
                                            // check so the guarded body is a fixed size (the value load is
                                            // variable-width; doing it here keeps the JE displacement
                                            // constant). The value load on the null path is harmless.
                self.load_to_rax(self.slot_of(base));
                self.load_to_rcx(self.slot_of(value));
                // TEST RAX,RAX ; JE +27 → skip (null receiver = no-op).
                self.buf.emit(&[0x48, 0x85, 0xC0]);
                self.buf.emit(&[0x74, 27]);
                // MOV dword [RAX + tag_off], 0   (Value::Int discriminant) — 10 bytes.
                self.buf.emit(&[0xC7, 0x80]);
                self.buf.emit(&tag_off.to_le_bytes());
                self.buf.emit(&0u32.to_le_bytes());
                // MOV dword [RAX + pay_off], ECX (Int payload) — 6 bytes.
                self.buf.emit(&[0x89, 0x88]);
                self.buf.emit(&pay_off.to_le_bytes());
                // MOV qword [RAX + high_off], 0  (clear high qword) — 11 bytes.
                self.buf.emit(&[0x48, 0xC7, 0x80]);
                self.buf.emit(&high_off.to_le_bytes());
                self.buf.emit(&0u32.to_le_bytes());
                // skip:  (10 + 6 + 11 = 27 bytes guarded — matches the JE rel8)
            }
            // invokestatic — dispatch via the `jit_invoke_dispatch` helper
            // (Gap B). inputs = [ctrl, mem, arg0, arg1, …]. The IR builder emits
            // this only for an oop-free method, so no object reference is ever
            // live across the call → no GC oop map needed. ABI (mirrors x64.rs):
            //   i64 helper(vm_ptr, info_ptr, args_ptr, num_args)
            // The Java args are marshalled contiguously into the frame staging
            // region (`args_ptr` → arg0, increasing addresses). A returned
            // `i64::MIN` means the callee threw: jump to the shared bail stub,
            // which returns the sentinel unchanged so the VM takes the pending
            // exception (the same protocol single-pass uses).
            Op::Call { info_ptr } => {
                let slot = self.alloc_slot(id);
                let num_args = node.inputs.len().saturating_sub(2);
                // 1. Marshal each Java arg into the staging region.
                for i in 0..num_args {
                    let arg = node.inputs[2 + i];
                    self.load_to_rax(self.slot_of(arg));
                    self.store_rax(self.args_stage_top_off - (i as i32) * 8);
                }
                // 2. Load the helper's four register arguments.
                self.load_reg_from_frame(CALL_ARG_REGS[0], self.context_slot_off); // vm_ptr
                self.emit_mov_reg_imm64(CALL_ARG_REGS[1], *info_ptr as u64); // info_ptr
                self.lea_reg_from_frame(CALL_ARG_REGS[2], self.args_stage_top_off); // args_ptr
                self.emit_mov_reg_imm64(CALL_ARG_REGS[3], num_args as u64); // num_args
                                                                            // 3. MOV RAX, invoke_dispatch ; CALL RAX.
                self.emit_mov_reg_imm64(RAX, self.invoke_dispatch as u64);
                self.buf.emit(&[0xFF, 0xD0]);
                // 4. Exception sentinel: CMP RAX, i64::MIN ; JE bail_stub.
                self.emit_mov_reg_imm64(R10, i64::MIN as u64);
                self.buf.emit(&[0x4C, 0x39, 0xD0]); // CMP RAX, R10
                self.buf.emit(&[0x0F, 0x84]); // JE rel32 (patched to the stub)
                let patch = self.buf.pos();
                self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                self.call_exc_patches.push(patch);
                // 5. Spill the return value (harmless for a void call: the slot
                //    is allocated but never read).
                self.store_rax(slot);
            }
            // ── FP value tier (inc 30) ───────────────────────────────────
            // A float/double constant is just its IEEE bit pattern written to
            // the result slot via a GPR immediate — no XMM. A float's payload
            // sits in the low 32 bits (high 32 left zero by the imm32 form);
            // every consumer reads it back with MOVSS (4 bytes).
            Op::ConstF(bits) => {
                let slot = self.alloc_slot(id);
                self.emit_mov_rax_imm64(*bits as i64);
                self.store_rax(slot);
            }
            // int → float / double. Load the int operand to EAX and convert.
            Op::I2F => {
                let slot = self.alloc_slot(id);
                self.load_to_rax(self.slot_of(node.inputs[0]));
                // CVTSI2SS XMM0, EAX
                self.buf.emit(&[0xF3, 0x0F, 0x2A, 0xC0]);
                self.fp_store(slot, XMM0, false);
            }
            Op::I2D => {
                let slot = self.alloc_slot(id);
                self.load_to_rax(self.slot_of(node.inputs[0]));
                // CVTSI2SD XMM0, EAX
                self.buf.emit(&[0xF2, 0x0F, 0x2A, 0xC0]);
                self.fp_store(slot, XMM0, true);
            }
            // long → float / double (64-bit source operand in RAX).
            Op::L2F => {
                let slot = self.alloc_slot(id);
                self.load_to_rax(self.slot_of(node.inputs[0]));
                // CVTSI2SS XMM0, RAX (REX.W)
                self.buf.emit(&[0xF3, 0x48, 0x0F, 0x2A, 0xC0]);
                self.fp_store(slot, XMM0, false);
            }
            Op::L2D => {
                let slot = self.alloc_slot(id);
                self.load_to_rax(self.slot_of(node.inputs[0]));
                // CVTSI2SD XMM0, RAX (REX.W)
                self.buf.emit(&[0xF2, 0x48, 0x0F, 0x2A, 0xC0]);
                self.fp_store(slot, XMM0, true);
            }
            // float → int / long (truncate toward zero, with the JVM
            // NaN→0 / overflow→MAX|MIN fixup). Source stays in XMM0 for the
            // fixup's sign/NaN test.
            Op::F2I => {
                let slot = self.alloc_slot(id);
                self.fp_load(XMM0, self.slot_of(node.inputs[0]), false);
                // CVTTSS2SI EAX, XMM0
                self.buf.emit(&[0xF3, 0x0F, 0x2C, 0xC0]);
                self.emit_fp_to_int_fixup(/* is_double */ false, /* is_long */ false);
                // Sign-extend EAX→RAX so the int slot matches the single-pass ABI.
                self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
                self.store_rax(slot);
            }
            Op::F2L => {
                let slot = self.alloc_slot(id);
                self.fp_load(XMM0, self.slot_of(node.inputs[0]), false);
                // CVTTSS2SI RAX, XMM0 (REX.W)
                self.buf.emit(&[0xF3, 0x48, 0x0F, 0x2C, 0xC0]);
                self.emit_fp_to_int_fixup(/* is_double */ false, /* is_long */ true);
                self.store_rax(slot);
            }
            // float → double.
            Op::F2D => {
                let slot = self.alloc_slot(id);
                self.fp_load(XMM0, self.slot_of(node.inputs[0]), false);
                // CVTSS2SD XMM0, XMM0
                self.buf.emit(&[0xF3, 0x0F, 0x5A, 0xC0]);
                self.fp_store(slot, XMM0, true);
            }
            // double → int / long (truncate toward zero, with the JVM fixup).
            Op::D2I => {
                let slot = self.alloc_slot(id);
                self.fp_load(XMM0, self.slot_of(node.inputs[0]), true);
                // CVTTSD2SI EAX, XMM0
                self.buf.emit(&[0xF2, 0x0F, 0x2C, 0xC0]);
                self.emit_fp_to_int_fixup(/* is_double */ true, /* is_long */ false);
                self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
                self.store_rax(slot);
            }
            Op::D2L => {
                let slot = self.alloc_slot(id);
                self.fp_load(XMM0, self.slot_of(node.inputs[0]), true);
                // CVTTSD2SI RAX, XMM0 (REX.W)
                self.buf.emit(&[0xF2, 0x48, 0x0F, 0x2C, 0xC0]);
                self.emit_fp_to_int_fixup(/* is_double */ true, /* is_long */ true);
                self.store_rax(slot);
            }
            // double → float.
            Op::D2F => {
                let slot = self.alloc_slot(id);
                self.fp_load(XMM0, self.slot_of(node.inputs[0]), true);
                // CVTSD2SS XMM0, XMM0
                self.buf.emit(&[0xF2, 0x0F, 0x5A, 0xC0]);
                self.fp_store(slot, XMM0, false);
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
            // Integer / long constants need no machine location. A cat-1 `int`
            // constant resolves to `Int`; a cat-2 `long` constant resolves to
            // `Long` (the resume builds a `Value::Long` with cat-2 two-slot
            // local placement — `real-frame-deopt` cat-2).
            Op::Const(v) => {
                if node.ty == IrType::Long {
                    FrameValue::Long(v)
                } else {
                    FrameValue::Int(v)
                }
            }
            // Float / double constant bits. FP-slot resume is a follow-up, so
            // the resume currently re-runs on a `Float` slot (safe) — the value
            // is carried for a future FP-aware resume.
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
                Self::typed_stack_slot(-off, node.ty)
            }
            _ => {
                let slot = self.node_slot[node_id as usize];
                if slot != 0 {
                    Self::typed_stack_slot(-slot, node.ty)
                } else {
                    // No machine location assigned (unscheduled / dead in this
                    // naive lowerer). A real resolver would never see this for
                    // a value that is live at the safepoint; first-cut fallback.
                    FrameValue::Undefined
                }
            }
        }
    }

    /// Encode a spilled value at `off` (relative to `rbp`) as a typed
    /// `FrameValue` location, driven by the IR node's value type
    /// (`real-frame-deopt` type source). A `Ref` slot becomes `StackSlotRef`
    /// (resolves to a `Value::Object`); a cat-1 `Int` slot stays `StackSlot`
    /// (resolves to `Value::Int`); a cat-2 `Long` slot becomes `StackSlotLong`
    /// (resolves to a `Value::Long` with cat-2 two-slot placement). `Double` and
    /// FP (`Float`) slots are `Unsupported` for now — the IR path does not
    /// compile float/double methods, and FP-slot/XMM resolution is a follow-up,
    /// so the resume falls back to the safe re-run path rather than truncate/
    /// mistype them. `Void`/`Control`/`Memory` are never live data slots, so they
    /// too map to `Unsupported`.
    fn typed_stack_slot(off: i32, ty: IrType) -> FrameValue {
        match ty {
            IrType::Ref => FrameValue::StackSlotRef(off),
            IrType::Int => FrameValue::StackSlot(off),
            IrType::Long => FrameValue::StackSlotLong(off),
            _ => FrameValue::Unsupported,
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

    /// Emit a div-by-zero deopt guard for an `Op::Div`/`Op::Rem` whose divisor
    /// was just loaded into RCX. If the divisor is zero, deopt to the
    /// interpreter at this bci, which re-executes the `idiv`/`irem` and throws
    /// `ArithmeticException` — instead of the raw `IDIV` faulting (#DE/SIGFPE),
    /// the latent crash this fixes. Only emitted when the bci has a safepoint
    /// snapshot (so the reconstructed frame carries the operand stack the
    /// interpreter needs to re-execute the division); a hand-built graph with
    /// no snapshot keeps the bare `IDIV`.
    fn emit_div_zero_guard(&mut self, ty: IrType, bytecode_pc: Option<usize>) {
        let bci = match bytecode_pc {
            Some(b) if self.graph.safepoints.iter().any(|s| s.bci == b) => b,
            _ => return,
        };
        // TEST ECX,ECX (int) / TEST RCX,RCX (long): ZF=1 when divisor == 0.
        if ty == IrType::Int {
            self.buf.emit(&[0x85, 0xC9]);
        } else {
            self.buf.emit(&[0x48, 0x85, 0xC9]);
        }
        self.emit_deopt_if_zero(bci, DeoptReason::DivByZero);
    }

    /// Emit the JVMS `MIN_VALUE / -1` overflow guard for an `Op::Div`/`Op::Rem`
    /// whose dividend is in RAX and divisor in RCX (after `emit_div_zero_guard`).
    /// A raw `IDIV` on `MIN / -1` raises `#DE` — the latent **hang** this fixes
    /// (the single-pass backend's `emit_safe_idiv` already guards this; the IR
    /// backend did not, so a JIT'd `idiv(Integer.MIN_VALUE, -1)` faulted and the
    /// fault handler spun). When `dividend == MIN && divisor == -1` we
    /// materialise the spec result (quotient == MIN, i.e. the dividend unchanged;
    /// remainder == 0) and `JMP` past the `IDIV`. Always safe to emit (pure
    /// inline branch, no deopt / no safepoint needed). Returns the position of
    /// the forward `JMP` rel32 the caller must patch to the post-`IDIV`
    /// continuation; the two `JNE`s to the `do_div` (`IDIV`) path are patched
    /// here. Uses R10 as scratch for the 64-bit MIN compare (the IR lowering
    /// never homes a value there).
    fn emit_div_overflow_guard(&mut self, ty: IrType, is_rem: bool) -> usize {
        // CMP dividend, MIN
        if ty == IrType::Int {
            // CMP EAX, imm32  (3D <imm32>)
            self.buf.emit_byte(0x3D);
            self.buf.emit(&(i32::MIN as u32).to_le_bytes());
        } else {
            // MOV R10, i64::MIN (49 BA <imm64>) ; CMP RAX, R10 (4C 39 D0)
            self.buf.emit(&[0x49, 0xBA]);
            self.buf.emit(&(i64::MIN as u64).to_le_bytes());
            self.buf.emit(&[0x4C, 0x39, 0xD0]);
        }
        // JNE do_div (0F 85 rel32)
        self.buf.emit(&[0x0F, 0x85]);
        let jne1 = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        // CMP divisor, -1
        if ty == IrType::Int {
            self.buf.emit(&[0x83, 0xF9, 0xFF]); // CMP ECX, -1
        } else {
            self.buf.emit(&[0x48, 0x83, 0xF9, 0xFF]); // CMP RCX, -1
        }
        // JNE do_div (0F 85 rel32)
        self.buf.emit(&[0x0F, 0x85]);
        let jne2 = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        // Materialise the overflow result (matches the zero-extended convention
        // a 32-bit IDIV leaves in RAX).
        if is_rem {
            // remainder == 0 → XOR EAX, EAX (zeros the full RAX).
            self.buf.emit(&[0x31, 0xC0]);
        } else if ty == IrType::Int {
            // quotient == MIN → MOV EAX, 0x80000000 (zero-extends into RAX).
            self.buf.emit_byte(0xB8);
            self.buf.emit(&(i32::MIN as u32).to_le_bytes());
        } else {
            // quotient == MIN → MOV RAX, i64::MIN.
            self.buf.emit(&[0x48, 0xB8]);
            self.buf.emit(&(i64::MIN as u64).to_le_bytes());
        }
        // JMP after (E9 rel32) — patched by the caller after the IDIV.
        self.buf.emit_byte(0xE9);
        let after_patch = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        // do_div: patch both JNEs to land here (the IDIV the caller emits next).
        let do_div = self.buf.pos();
        for p in [jne1, jne2] {
            let rel = do_div as i32 - (p as i32 + 4);
            self.buf
                .try_patch_i32(p, rel)
                .expect("div-overflow JNE patch in-bounds");
        }
        after_patch
    }

    /// Patch the forward `JMP` emitted by [`emit_div_overflow_guard`] to the
    /// current position (the post-`IDIV` continuation, just before the result
    /// is stored).
    fn patch_div_overflow_after(&mut self, after_patch: usize) {
        let cont = self.buf.pos();
        let rel = cont as i32 - (after_patch as i32 + 4);
        self.buf
            .try_patch_i32(after_patch, rel)
            .expect("div-overflow JMP patch in-bounds");
    }

    /// Emit a deopt-on-zero branch, given the caller has already emitted a
    /// `TEST` that sets `ZF=1` exactly when the deopt condition holds (the
    /// tested value was zero). Builds + boxes a `DeoptimizationPoint` for `bci`
    /// (frame state from its safepoint snapshot), then emits
    /// `JNZ continue; <mov DEOPT_ARG0, point; JMP deopt_stub>; continue:`.
    /// Shares the Phase-A deopt stub via `deopt_stub_patches`.
    ///
    /// NOTE: on Windows `DEOPT_ARG0` is RCX, which a div/rem site uses for the
    /// divisor — but the `mov` only executes on the deopt branch (after the
    /// `JNZ`), so the fall-through path keeps RCX intact for the `IDIV`.
    fn emit_deopt_if_zero(&mut self, bci: usize, reason: DeoptReason) {
        let frame_state = self.resolve_frame_state_for_bci(bci);
        let point = Box::new(DeoptimizationPoint {
            native_offset: self.buf.pos() as u32,
            bci: bci as u32,
            reason,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state,
        });
        let point_ptr = point.as_ref() as *const DeoptimizationPoint as u64;
        self.deopt_boxes.push(point);
        // JNZ continue (value != 0 → skip deopt).
        self.buf.emit(&[0x0F, 0x85]);
        let jnz_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        // Deopt path: load the point pointer into arg0, JMP to the shared stub.
        self.emit_mov_reg_imm64(DEOPT_ARG0, point_ptr);
        self.buf.emit_byte(0xE9);
        let jmp_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.deopt_stub_patches.push(jmp_patch);
        // continue:
        let cont = self.buf.pos();
        let rel = cont as i32 - (jnz_patch as i32 + 4);
        self.buf
            .try_patch_i32(jnz_patch, rel)
            .expect("deopt-if-zero JNZ patch in-bounds");
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

    /// Gap B: emit the single shared call-exception bail stub (if any `Op::Call`
    /// emitted a sentinel check) and patch every dispatch site's `JE` to it. On
    /// entry `RAX` already holds the `i64::MIN` sentinel the helper returned when
    /// the callee threw; the stub just runs the epilogue, returning the sentinel
    /// so the VM's post-JIT path takes the pending exception (the same protocol
    /// the single-pass backend uses).
    fn emit_call_exc_stub(&mut self) {
        if self.call_exc_patches.is_empty() {
            return;
        }
        let stub_off = self.buf.pos();
        self.buf.emit(&[0x48, 0x81, 0xC4]); // add rsp, frame_size
        self.buf.emit(&self.frame_size.to_le_bytes());
        self.buf.emit_byte(0x5D); // pop rbp
        self.buf.emit_byte(0xC3); // ret
        let patches = std::mem::take(&mut self.call_exc_patches);
        for p in patches {
            let rel = stub_off as i32 - (p as i32 + 4);
            self.buf
                .try_patch_i32(p, rel)
                .expect("call-exc JE patch in-bounds");
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
    helpers: &JitRuntimeHelpers,
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
        helpers,
    );

    // Gap B: a `needs_context` method (one containing an `Op::Call`) receives the
    // VM pointer as a hidden first arg, but only `abi_regs.len()` integer
    // registers carry incoming args. If `1 + num_params` would spill a param to
    // the stack, the prologue can't load it — bail to single-pass (the safety
    // net) rather than mis-read the param. `abi_regs` is 4 on Win64, 6 on SysV.
    if lowerer.needs_context {
        #[cfg(target_os = "windows")]
        let abi_len = 4usize;
        #[cfg(not(target_os = "windows"))]
        let abi_len = 6usize;
        if 1 + num_params > abi_len {
            return None;
        }
    }

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
    // Gap B: emit the shared call-exception bail stub after the body so each
    // dispatch site's sentinel `JE` reaches it.
    lowerer.emit_call_exc_stub();

    lowerer.patch_branches();

    // real-frame-deopt (step 2): resolve recorded safepoint snapshots into
    // native-offset-keyed DeoptimizationPoints before the buffer is consumed.
    // Emit-and-discard: nothing reads these yet, so codegen is unchanged.
    let deopt_points = lowerer.build_deopt_points();
    let deopt_boxes = std::mem::take(&mut lowerer.deopt_boxes);
    // Gap B: a method containing an `Op::Call` takes the VM context pointer as a
    // hidden first arg, so it must be invoked via `try_call_with_context`.
    let needs_context = lowerer.needs_context;

    let buf = lowerer.buf;
    let _code_size = buf.pos();

    let mut cm = CompiledMethod::new(buf);
    cm.deopt_points = deopt_points;
    cm._deopt_point_boxes = deopt_boxes;
    if needs_context {
        cm.needs_context = true;
    }
    Some(cm)
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::IrBuilder;
    use crate::ir_optimize;
    use crate::ir_schedule;

    /// All-zero helpers for tests that contain no `Op::Call` (no helper pointer
    /// is ever dereferenced). SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` with
    /// all-integer (usize) fields, so an all-zero bit pattern is a valid value.
    fn no_helpers() -> JitRuntimeHelpers {
        unsafe { std::mem::zeroed() }
    }

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
        lower(&graph, &schedule, num_params, num_locals, &no_helpers())
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
        lower(&graph, &schedule, num_params, num_locals, &no_helpers()).expect("lower")
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
        let method = lower(&graph, &schedule, 2, 2, &no_helpers()).expect("lower guarded method");

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
        let method = lower(&graph, &schedule, 2, 2, &no_helpers()).expect("lower cmp graph");

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

    // do-while: the back-edge is an `if_icmplt` (not a goto), and the loop
    // header self-loops (the condition is at the bottom). Exercises the
    // if-as-back-edge path + a block whose true edge targets its own head.
    #[test]
    fn test_lower_do_while_sum() {
        // int f(int n){ int s=0,i=0; do { s+=i; i++; } while(i<n); return s; }
        //  0:iconst_0 1:istore_1 2:iconst_0 3:istore_2
        //  4:iload_1 5:iload_2 6:iadd 7:istore_1 8:iinc 2,1
        // 11:iload_2 12:iload_0 13:if_icmplt 4  16:iload_1 17:ireturn
        let code = [
            0x03, 0x3c, 0x03, 0x3d, 0x1b, 0x1c, 0x60, 0x3c, 0x84, 0x02, 0x01, 0x1c, 0x1a, 0xa1,
            0xff, 0xf7, 0x1b, 0xac, 0, 0,
        ];
        let cm = compile_via_ir(&code, 18, 1, 3).expect("do-while compiles via IR");
        let f = |n: i64| unsafe { cm.try_call(&[n]).expect("call") };
        assert_eq!(f(5), 10, "runs i=0..4 → 0+1+2+3+4 = 10");
        assert_eq!(f(1), 0, "runs once (i=0) → 0");
        assert_eq!(f(0), 0, "do-while runs once even at n=0 → s=0");
    }

    // Nested counted loops: two loop headers, the inner nested in the outer.
    #[test]
    fn test_lower_nested_loops() {
        // int f(int n){ int c=0; for(i=0;i<n;i++) for(j=0;j<n;j++) c++; return c; }
        //  0:iconst_0 1:istore_1            // c=0
        //  2:iconst_0 3:istore_2            // i=0
        //  4:iload_2 5:iload_0 6:if_icmpge 28   // outer header
        //  9:iconst_0 10:istore_3           // j=0
        // 11:iload_3 12:iload_0 13:if_icmpge 22 // inner header
        // 16:iinc 1,1  19:iinc 3,1  22? ...
        // Layout carefully below.
        //  0:03 1:3c 2:03 3:3d
        //  4:1c 5:1a 6:a2 00 16(=22? we need offset) ...
        // Compute: outer if_icmpge at 6 must exit past the outer iinc/goto.
        //  4: iload_2        1c            outer header
        //  5: iload_0        1a
        //  6: if_icmpge 31   a2 00 19       (6+25=31)
        //  9: iconst_0       03            j=0
        // 10: istore_3       3e
        // 11: iload_3        1d            inner header
        // 12: iload_0        1a
        // 13: if_icmpge 25   a2 00 0c       (13+12=25)
        // 16: iinc 1,1       84 01 01       c++
        // 19: iinc 3,1       84 03 01       j++
        // 22: goto 11        a7 ff f5       (22-11=11)
        // 25: iinc 2,1       84 02 01       i++
        // 28: goto 4         a7 ff e8       (28-24=4)
        // 31: iload_1        1b
        // 32: ireturn        ac
        let code = [
            0x03, 0x3c, 0x03, 0x3d, // 0..3
            0x1c, 0x1a, 0xa2, 0x00, 0x19, // 4: outer header, if_icmpge 31
            0x03, 0x3e, // 9: j=0
            0x1d, 0x1a, 0xa2, 0x00, 0x0c, // 11: inner header, if_icmpge 25
            0x84, 0x01, 0x01, // 16: iinc c
            0x84, 0x03, 0x01, // 19: iinc j
            0xa7, 0xff, 0xf5, // 22: goto 11
            0x84, 0x02, 0x01, // 25: iinc i
            0xa7, 0xff, 0xe8, // 28: goto 4
            0x1b, 0xac, // 31: iload_1; ireturn
            0, 0,
        ];
        let cm = compile_via_ir(&code, 33, 1, 4).expect("nested loops compile via IR");
        let f = |n: i64| unsafe { cm.try_call(&[n]).expect("call") };
        assert_eq!(f(3), 9, "3*3 = 9");
        assert_eq!(f(5), 25, "5*5 = 25");
        assert_eq!(f(0), 0, "no iterations");
        assert_eq!(f(1), 1, "1*1 = 1");
    }

    // ── real-frame-deopt fires end-to-end: div-by-zero guard ─────────────
    // Proves the trigger half: an IR-compiled `a/b` deopts (instead of the raw
    // IDIV faulting) when b==0, with the reconstructed frame carrying the live
    // operands at the idiv bci so the interpreter can re-execute and throw
    // ArithmeticException. (The VM-side resume is wired separately, gated.)
    #[test]
    fn test_lower_div_by_zero_deopts() {
        use crate::deopt::{take_last_deopt, FrameValue};
        // int f(int a, int b){ return a / b; }  — iload_0; iload_1; idiv; ireturn
        let code = [0x1a, 0x1b, 0x6c, 0xac, 0, 0];
        let cm = compile_via_ir(&code, 4, 2, 2).expect("div compiles via IR");

        let _ = take_last_deopt(); // clear any stale state
                                   // divisor != 0 → normal result, no deopt.
        let ok = unsafe { cm.try_call(&[20, 4]).expect("call (b != 0)") };
        assert_eq!(ok, 5, "20 / 4 = 5");
        assert!(take_last_deopt().is_none(), "no deopt when divisor != 0");

        // divisor == 0 → deopt (sentinel + reconstructed frame), NOT a #DE fault.
        let sentinel = unsafe { cm.try_call(&[20, 0]).expect("call (b == 0)") };
        assert_eq!(sentinel, i64::MIN, "div by zero → deopt sentinel");
        let frame = take_last_deopt().expect("deopt reconstructed a frame");
        // idiv is at bci 2; resume there with [a, b] live on the operand stack
        // so the interpreter re-executes the division and throws.
        assert_eq!(frame.bci, 2, "resume at the idiv bci");
        assert_eq!(
            frame.stack,
            vec![FrameValue::Int(20), FrameValue::Int(0)],
            "operands restored for re-execution",
        );
    }

    #[test]
    fn test_lower_ldiv_by_zero_reconstructs_long_frame() {
        use crate::deopt::{take_last_deopt, FrameValue};
        use crate::ir::{IrBuilder, IrType};
        // long f(long a, long b){ return a / b; }
        //   lload_0; lload_2; ldiv; lreturn   (+ 2 trailing padding bytes)
        // Proves a `long` live at the div guard reconstructs as a full-64-bit
        // FrameValue::Long (cat-2 width on resume), not a truncated Int.
        let code = [0x1e, 0x20, 0x6d, 0xad, 0, 0];
        let mut builder = IrBuilder::new(2, 4); // 2 long params (a@0-1, b@2-3)
        builder.set_param_types(&[IrType::Long, IrType::Long]);
        let mut graph = builder.build(&code, 4).expect("ldiv IR build");
        ir_optimize::optimize(&mut graph);
        let schedule = ir_schedule::schedule(&graph);
        let cm = lower(&graph, &schedule, 2, 4, &no_helpers()).expect("lower ldiv");

        let _ = take_last_deopt(); // clear any stale state
                                   // divisor != 0 → normal full-64-bit result, no deopt. A 32-bit IDIV
                                   // would mishandle this dividend (> i32::MAX).
        let ok = unsafe { cm.try_call(&[0x1_0000_0000, 2]).expect("call (b != 0)") };
        assert_eq!(ok, 0x8000_0000, "0x1_0000_0000 / 2 (genuinely 64-bit)");
        assert!(take_last_deopt().is_none(), "no deopt when divisor != 0");

        // divisor == 0 → deopt; the reconstructed frame must carry the two LONG
        // operands as FrameValue::Long (full 64 bits) so the interpreter resumes
        // at the ldiv bci and re-executes it (throwing ArithmeticException).
        let sentinel = unsafe { cm.try_call(&[0x7_0000_0000, 0]).expect("call (b == 0)") };
        assert_eq!(sentinel, i64::MIN, "ldiv by zero → deopt sentinel");
        let frame = take_last_deopt().expect("deopt reconstructed a frame");
        assert_eq!(frame.bci, 2, "resume at the ldiv bci");
        assert_eq!(
            frame.stack,
            vec![FrameValue::Long(0x7_0000_0000), FrameValue::Long(0)],
            "long operands restored as full-64-bit FrameValue::Long",
        );
        // Locals: a@0 and b@2 are longs (StackSlotLong → Long); the high-half
        // slots 1/3 are dummies (never read by the interpreter).
        assert_eq!(frame.locals[0], FrameValue::Long(0x7_0000_0000));
        assert_eq!(frame.locals[2], FrameValue::Long(0));
    }
}
