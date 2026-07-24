// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lower the scheduled IR graph to x86-64 machine code.
//!
//! Walks the scheduled basic blocks, emits native instructions for each
//! IR node, and patches forward branches.

use std::collections::HashMap;

use super::ir::{Graph, IrType, MemKind, NodeId, Op, SafepointSnapshot, NO_NODE};
use super::ir_schedule::Schedule;
use super::{CompiledMethod, ExecutableBuffer, JitInvokeInfo, JitRuntimeHelpers};
use crate::deopt::{
    ir_deopt_entry, DeoptAction, DeoptReason, DeoptimizationPoint, FrameState, FrameValue,
    VirtualObjectState,
};
use cratonvm_types::{ARRAY_LENGTH_OFFSET, FIELD_CELL_PAYLOAD32_OFFSET, HEADER_SIZE, SLOT_SIZE};

// ── Guard-surviving scalar replacement (producer side) ───────────────
//
// When escape analysis scalar-replaces an `Op::New` (the allocation is elided,
// its field loads redirected to the stored values), the object no longer exists
// in the JIT frame — but it may still be live at a deopt point. The default
// behaviour resolves its (now-`Op::Dead`) snapshot slot to `FrameValue::Undefined`,
// forcing a whole-method re-run. With this map threaded into the lowerer, such a
// slot instead lowers to a `FrameValue::VirtualObject`, which the VM's
// `materialize_virtual_objects` consumer rebuilds on a precise resume.
//
// Built by `lib.rs::build_scalar_replacement_map` from the escape-analysis result
// (so `class_id`/`num_fields`/`field_values` are captured before the `Op::New` is
// marked dead) and passed to `lower_with_scalar_deopt`. `None` (the default)
// preserves the exact prior behaviour — byte-identical default builds.

/// Per-scalar-replaced-object metadata the deopt producer needs to emit a
/// `FrameValue::VirtualObject`. Keyed (in [`ScalarReplacementMap`]) by the IR
/// `NodeId` of the eliminated `Op::New`, which is also used as the object's
/// stable [`crate::deopt::VirtualObjectState::id`] within a deopt frame.
pub struct VirtualObjectInfo {
    pub class_id: u32,
    pub num_fields: usize,
    /// Per field index: the IR value node the field holds, or `None` for a field
    /// never stored (resolves to the object's zero default). Admitted allocations
    /// have no non-zero primitive `<init>`, so `None` ⇒ `0` is sound.
    pub field_values: Vec<Option<NodeId>>,
    /// Control input of the eliminated `Op::New` (its block's control node),
    /// captured before EA cleared the dead node's inputs. The producer requires
    /// it to strictly dominate a deopt point (the object must have been
    /// allocated by then). Read via the live control node, not the now-dead New.
    pub new_ctrl: NodeId,
    /// Control inputs of the eliminated field stores (each store's block control
    /// node), captured before EA cleared them. The producer's temporal gate
    /// requires every one to strictly dominate a deopt point before emitting a
    /// `VirtualObject` there — else a deopt *before* a store would materialize
    /// the post-store value instead of the field's actual (earlier) value.
    pub store_ctrls: Vec<NodeId>,
}

/// Maps each scalar-replaced `Op::New` (by IR `NodeId`) to its
/// [`VirtualObjectInfo`]. Empty / `None` ⇒ no guard-surviving SR emission.
#[derive(Default)]
pub struct ScalarReplacementMap {
    pub objects: HashMap<NodeId, VirtualObjectInfo>,
}

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
const R11: u8 = 11;

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
    /// Soundness latch (ES SortingDigestTests -Jit on): set when `slot_of`
    /// is asked for a node that never went through `alloc_slot`. `node_slot`
    /// is zero-initialised and `first_spill > 0`, so a 0 readback means the
    /// scheduler never placed the node in an emitted block (observed: the
    /// pc17 ArrayLoad(Double) feeding a GVN-collapsed loop phi in
    /// DualPivotQuicksort.insertionSort) — emitting `[rbp - 0]` would read
    /// the saved caller RBP as a data value (heap addresses stored into
    /// double[] elements, silent mis-sorts). The lowering entry point checks
    /// this latch and bails to the single-pass backend instead.
    unallocated_slot_use: std::cell::Cell<bool>,
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
    lambda_int_to_double: usize,
    /// Address of the `jit_dispatch_threw` peek helper. Baked into a `J`/`D`
    /// (long/double) call site's post-invoke check on the rare `RAX == i64::MIN`
    /// branch to disambiguate a genuine callee exception/deopt from a legitimate
    /// `Long.MIN_VALUE` return (see `lower_call`'s sentinel sequence). 0 if no
    /// calls / not wired (int/ref/void sites never consult it).
    dispatch_threw: usize,
    /// Catchable native-stack overflow guard for direct self-recursion.
    self_call_stack_guard: usize,
    /// IR FP tier (Slice A) — address of the `jit_frem` / `jit_drem` runtime
    /// helpers (`extern "C" fn(f32,f32)->f32` / `fn(f64,f64)->f64`). Baked into
    /// an `Op::Rem` Float/Double site as `MOV RAX,imm64 ; CALL RAX` with the two
    /// operands already in XMM0/XMM1 and the result read back from XMM0.
    frem: usize,
    drem: usize,
    /// Address of the checked `jit_getfield` helper. When present, `Op::Load`
    /// instance-field reads route through it so receivers are validated against
    /// the live heap before any object-header dereference.
    getfield: usize,
    /// Cooperative GC poll flag and no-argument slow path. IR values are
    /// canonicalized in frame slots, so the slow-path call needs no spill.
    safepoint_flag_addr: usize,
    safepoint_slow_path: usize,
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
    /// fib44-fix follow-up: native offsets of the rel32 operand of each direct
    /// self-recursive `CALL` (invoke_kind 4), patched at finalize to target the
    /// method's own entry (code offset 0). See `lower_self_call` / Op::Call.
    self_call_patches: Vec<usize>,
    /// wire-tiered-manager Step 4 (PGO handoff C1 → C2): per-bytecode-PC branch
    /// bias, keyed by the conditional-branch instruction's bytecode PC (the same
    /// key the IR builder stamps on each `Op::If` via `Node::bytecode_pc`). Value
    /// `true` = the branch is usually TAKEN, `false` = usually NOT taken; an
    /// absent PC is inconclusive. Only `Some(false)` (usually-not-taken) changes
    /// codegen — see `lower_terminator`'s `Op::If` arm. Empty (the default, and
    /// whenever profiling is off) ⇒ every `Op::If` keeps its historical layout
    /// byte-for-byte.
    branch_hints: &'a HashMap<usize, bool>,
    /// Guard-surviving scalar replacement: metadata for each scalar-replaced
    /// `Op::New` so a deopt snapshot slot holding it lowers to a
    /// `FrameValue::VirtualObject`. `None` ⇒ disabled (byte-identical default).
    sr_map: Option<&'a ScalarReplacementMap>,
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
        branch_hints: &'a HashMap<usize, bool>,
        sr_map: Option<&'a ScalarReplacementMap>,
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
            if matches!(n.op, Op::LambdaIntToDouble) {
                needs_context = true;
            }
            if helpers.getfield != 0 && matches!(n.op, Op::Load(_)) {
                needs_context = true;
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
            unallocated_slot_use: std::cell::Cell::new(false),
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
            lambda_int_to_double: helpers.lambda_int_to_double,
            dispatch_threw: helpers.dispatch_threw,
            self_call_stack_guard: helpers.self_call_stack_guard,
            frem: helpers.jit_frem,
            drem: helpers.jit_drem,
            getfield: helpers.getfield,
            safepoint_flag_addr: helpers.safepoint_flag_addr,
            safepoint_slow_path: helpers.safepoint_slow_path,
            needs_context,
            context_slot_off,
            args_stage_top_off,
            spill_cap_off,
            call_exc_patches: Vec::new(),
            self_call_patches: Vec::new(),
            branch_hints,
            sr_map,
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
    /// A `0` readback means the node was never `alloc_slot`'d — see
    /// `unallocated_slot_use`; the latch makes the whole lowering bail
    /// rather than emit a `[rbp - 0]` access to the saved caller RBP.
    fn slot_of(&self, id: NodeId) -> i32 {
        let s = self.node_slot[id as usize];
        if s == 0 {
            self.unallocated_slot_use.set(true);
            if std::env::var_os("CRATONVM_DBG_IRSLOT").is_some() {
                let n = &self.graph.nodes[id as usize];
                eprintln!(
                    "[irslot] UNALLOCATED node={} op={:?} ty={:?} inputs={:?} pc={:?}",
                    id, n.op, n.ty, n.inputs, n.bytecode_pc
                );
            }
        }
        s
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

    /// Emit the default-on cooperative poll used at method entries and loop
    /// back-edges. The lowerer keeps all live values in frame slots, so the
    /// no-argument slow path may be called directly.
    fn emit_safepoint_poll(&mut self) {
        let enabled = std::env::var_os("CRATONVM_JIT_SAFEPOINT_POLLS")
            .and_then(|v| v.into_string().ok())
            .is_none_or(|v| v != "0");
        if !enabled || self.safepoint_flag_addr == 0 || self.safepoint_slow_path == 0 {
            return;
        }
        self.emit_mov_reg_imm64(R11, self.safepoint_flag_addr as u64);
        self.buf.emit(&[0x41, 0xF6, 0x03, 0xFF]); // TEST byte ptr [R11], 0xff
        self.buf.emit(&[0x0F, 0x84]); // JZ .clear
        let clear_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.emit_mov_reg_imm64(RAX, self.safepoint_slow_path as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        let rel = self.buf.pos() as i32 - (clear_patch as i32 + 4);
        self.buf
            .try_patch_i32(clear_patch, rel)
            .expect("IR safepoint poll patch in-bounds");
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
                if succ_block <= block_idx {
                    self.emit_safepoint_poll();
                }
                self.emit_phi_copies(block_idx, succ_block);
                self.buf.emit_byte(0xE9); // JMP succ_block
                let patch_pos = self.buf.pos();
                self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                self.branch_patches.push((patch_pos, succ_block));
            }
        }
    }

    /// fib44-fix follow-up: emit a self-recursive call as a DIRECT `CALL` to this
    /// method's own entry (code offset 0), instead of the generic
    /// `jit_invoke_dispatch` helper. Only reached for an `Op::Call` whose
    /// `JitInvokeInfo.invoke_kind == 4` (the eligibility loop sets that when
    /// `CRATONVM_JIT_IR_SELFREC_DIRECT` is on for a supported self-recursive
    /// static call). `inputs` is the call node's `inputs` (`[ctrl, mem, args…]`), `slot`
    /// its result slot, `num_args` its Java arg count.
    ///
    /// SAFETY of a direct call vs. the C-ABI dispatch helper: the IR method is
    /// itself `extern "C"` (called from Rust via `try_call_with_context`), so it
    /// preserves the platform callee-saved registers — a self-call is just a call
    /// to that same ABI-compliant function.
    fn emit_self_recursive_call(&mut self, inputs: &[NodeId], slot: i32, num_args: usize) {
        // Preserve Java's catchable StackOverflowError semantics without a
        // helper call in every recursive frame. Sample once whenever the next
        // frame can cross a 64 KiB native-stack boundary. The runtime guard's
        // floor reserves 1 MiB, so even a check delayed by one full stride still
        // leaves at least 960 KiB for exception construction and unwinding.
        //
        //   low = RSP & 0xffff
        //   if low > frame_size + call/prologue bytes: skip helper
        self.buf.emit(&[0x48, 0x89, 0xE0]); // MOV RAX, RSP
        self.buf.emit_byte(0x25); // AND EAX, imm32
        self.buf.emit(&0xffffu32.to_le_bytes());
        self.buf.emit_byte(0x3D); // CMP EAX, imm32
        let next_frame_span = (self.frame_size as u32).saturating_add(16).min(0xffff);
        self.buf.emit(&next_frame_span.to_le_bytes());
        self.buf.emit(&[0x0F, 0x87]); // JA .guard_ok
        let fast_skip_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        if self.self_call_stack_guard != 0 {
            self.load_reg_from_frame(CALL_ARG_REGS[0], self.context_slot_off);
            self.emit_mov_reg_imm64(RAX, self.self_call_stack_guard as u64);
            self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
            self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
            self.buf.emit(&[0x0F, 0x85]); // JNE shared bail stub
            let patch = self.buf.pos();
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
            self.call_exc_patches.push(patch);
        }
        let rel = self.buf.pos() as i32 - (fast_skip_patch as i32 + 4);
        self.buf
            .try_patch_i32(fast_skip_patch, rel)
            .expect("ir_lower self-call stack-sample patch in-bounds");

        // Marshal args into this method's OWN entry ABI — IDENTICAL to the
        // register list `emit_prologue` reads incoming args from: abi[0] = the
        // hidden VM context pointer, abi[1 + i] = Java arg i. Each source is a
        // frame slot (memory), so loading straight into the abi registers cannot
        // inter-clobber. `1 + num_args <= abi.len()` is guaranteed by the
        // needs_context bail in `lower()`, so no arg spills off the register file.
        #[cfg(target_os = "windows")]
        let abi: &[u8] = &[1, 2, 8, 9]; // RCX, RDX, R8, R9
        #[cfg(not(target_os = "windows"))]
        let abi: &[u8] = &[7, 6, 2, 1, 8, 9]; // RDI, RSI, RDX, RCX, R8, R9
        self.load_reg_from_frame(abi[0], self.context_slot_off); // vm_ptr
        for i in 0..num_args {
            let arg = inputs[2 + i];
            self.load_reg_from_frame(abi[1 + i], self.slot_of(arg)); // Java arg i
        }
        // Direct CALL rel32 to entry (code offset 0), patched at finalize.
        self.buf.emit(&[0xE8]);
        let patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.self_call_patches.push(patch);
        // Exception/deopt sentinel — identical to the dispatch path's return
        // check. Integer/reference results cannot equal the full-width sentinel;
        // wide returns can legitimately carry those bits (for example
        // Long.MIN_VALUE), so on `RAX == i64::MIN` peek the out-of-band signal
        // via `dispatch_threw` and bail only when one is pending.
        self.emit_mov_reg_imm64(R10, i64::MIN as u64);
        self.buf.emit(&[0x4C, 0x39, 0xD0]); // CMP RAX, R10
        self.buf.emit(&[0x0F, 0x85]); // JNE .keep
        let keep_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.emit_mov_reg_imm64(RAX, self.dispatch_threw as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        self.emit_mov_reg_imm64(RAX, i64::MIN as u64); // restore (MOV preserves ZF)
        self.buf.emit(&[0x0F, 0x85]); // JNE bail_stub
        let exc_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.call_exc_patches.push(exc_patch);
        // .keep:
        let keep_off = self.buf.pos();
        let rel = keep_off as i32 - (keep_patch as i32 + 4);
        self.buf
            .try_patch_i32(keep_patch, rel)
            .expect("ir_lower self-call sentinel keep patch in-bounds");
        // Spill the return value.
        self.store_rax(slot);
    }

    /// fib44-fix follow-up: patch every direct self-recursive `CALL` (invoke_kind
    /// 4) so its rel32 targets this method's own entry — code offset 0.
    fn patch_self_calls(&mut self) {
        let patches = std::mem::take(&mut self.self_call_patches);
        for p in patches {
            // rel32 = target - (rel32_field_offset + 4); target = entry = 0.
            let rel = 0i32 - (p as i32 + 4);
            self.buf
                .try_patch_i32(p, rel)
                .expect("ir_lower self-call rel32 patch in-bounds");
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
                // FP remainder (`frem`/`drem`) is `fmod`-style with no single SSE
                // instruction, so it is lowered as a CALL to the jit_frem/jit_drem
                // runtime helper. The float ABI passes the two args in XMM0/XMM1
                // and returns in XMM0 on both Win64 and SysV — which is exactly
                // the IR's own XMM scratch convention — so no register shuffling
                // is needed: load the operands, MOV RAX,helper ; CALL RAX, store
                // the XMM0 result. The 32-byte Win64 shadow space and 16-byte
                // call alignment are reserved unconditionally by the frame layout
                // (see `Lowerer::new`), so this CALL is safe even in an otherwise
                // call-free method. The helper never throws/deopts (IEEE `fmod`
                // has no exceptional result — `x % 0.0` is NaN, not a trap), so
                // there is NO exception-sentinel check, unlike `Op::Call`.
                if matches!(node.ty, IrType::Float | IrType::Double) {
                    let is_d = node.ty == IrType::Double;
                    self.fp_load(XMM0, self.slot_of(node.inputs[0]), is_d); // a
                    self.fp_load(XMM1, self.slot_of(node.inputs[1]), is_d); // b
                    let helper = if is_d { self.drem } else { self.frem };
                    self.emit_mov_reg_imm64(RAX, helper as u64);
                    self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                    self.fp_store(slot, XMM0, is_d);
                    return;
                }
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
            Op::FCmp {
                double,
                nan_greater,
            } => {
                // FP 3-way compare → int {-1,0,1}, mirroring `Op::LCmp` but via
                // `ucomis` with the JVMS NaN-unordered rule. Branchless:
                // `result = AL - DL` where the operand order + SETcc choice put
                // a NaN operand on +1 (`cmpg`) or -1 (`cmpl`). `ucomis` raises
                // CF on BOTH "below" and "unordered", which is what makes the
                // NaN case fall out for free:
                //   cmpl: UCOMIS a,b ; AL=SETA(a>b) ; DL=SETB(a<b OR NaN)
                //         ⇒ AL-DL = {a>b:+1, a<b:-1, eq:0, NaN:-1}.
                //   cmpg: UCOMIS b,a ; AL=SETB((a>b) OR NaN) ; DL=SETA(a<b)
                //         ⇒ AL-DL = {a>b:+1, a<b:-1, eq:0, NaN:+1}.
                let is_d = *double;
                let slot = self.alloc_slot(id);
                self.fp_load(XMM0, self.slot_of(node.inputs[0]), is_d); // a
                self.fp_load(XMM1, self.slot_of(node.inputs[1]), is_d); // b
                if *nan_greater {
                    // UCOMIS XMM1, XMM0 (compare b vs a) — ModRM C8.
                    if is_d {
                        self.buf.emit(&[0x66, 0x0F, 0x2E, 0xC8]);
                    } else {
                        self.buf.emit(&[0x0F, 0x2E, 0xC8]);
                    }
                    self.buf.emit(&[0x0F, 0x92, 0xC0]); // SETB AL  ((a>b) OR NaN)
                    self.buf.emit(&[0x0F, 0x97, 0xC2]); // SETA DL  (a<b)
                } else {
                    // UCOMIS XMM0, XMM1 (compare a vs b) — ModRM C1.
                    if is_d {
                        self.buf.emit(&[0x66, 0x0F, 0x2E, 0xC1]);
                    } else {
                        self.buf.emit(&[0x0F, 0x2E, 0xC1]);
                    }
                    self.buf.emit(&[0x0F, 0x97, 0xC0]); // SETA AL  (a>b)
                    self.buf.emit(&[0x0F, 0x92, 0xC2]); // SETB DL  (a<b OR NaN)
                }
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
                if self.getfield != 0 {
                    self.load_reg_from_frame(CALL_ARG_REGS[0], self.context_slot_off);
                    self.load_reg_from_frame(CALL_ARG_REGS[1], self.slot_of(base));
                    self.emit_mov_reg_imm64(CALL_ARG_REGS[2], field_index as i64 as u64);
                    self.emit_mov_reg_imm64(RAX, self.getfield as u64);
                    self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                                                  // The checked `jit_getfield` helper returns the `i64::MIN`
                                                  // deopt/NPE sentinel (with the pending-NPE flag set) on a bad
                                                  // receiver instead of a legitimate field value. `Op::Load`
                                                  // only ever represents an int-category field (see the doc
                                                  // comment above), where `i64::MIN` can never be a genuine
                                                  // result, so a plain compare-and-bail is unambiguous — mirrors
                                                  // the non-J/D branch of `Op::Call`'s post-dispatch check
                                                  // below. Without this, a bad receiver silently corrupts
                                                  // execution instead of throwing (crash → hang conversion).
                    self.emit_mov_reg_imm64(R10, i64::MIN as u64);
                    self.buf.emit(&[0x4C, 0x39, 0xD0]); // CMP RAX, R10
                    self.buf.emit(&[0x0F, 0x84]); // JE rel32 → shared bail stub
                    let exc_patch = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    self.call_exc_patches.push(exc_patch);
                    self.store_rax(slot);
                } else {
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
            // FP array element load (Slice B) — `faload`/`daload`. inputs =
            // [ctrl, mem, array, index]. Load the array pointer to RAX and the
            // index to RCX (the layout the SIB access expects), emit the JVMS
            // null + bounds deopt guards, then `MOVSS`/`MOVSD` the element from
            // `[RAX + RCX*elem_size + HEADER_SIZE]` into XMM0 and spill it. The
            // index slot holds a sign-extended int; the 32-bit unsigned bounds
            // CMP (in the guard) rejects a negative index before the SIB uses
            // the 64-bit RCX (whose high half is then zero for an in-bounds
            // index). FP element accesses never alias the int field cells, but
            // the memory-token edge still serialises them with neighbours.
            Op::ArrayLoad(kind) => {
                let slot = self.alloc_slot(id);
                let is_d = matches!(kind, MemKind::Double);
                let bci = node.bytecode_pc.unwrap_or(0);
                self.load_to_rax(self.slot_of(node.inputs[2])); // array → RAX
                self.load_to_rcx(self.slot_of(node.inputs[3])); // index → RCX
                self.emit_array_null_bounds_guards(bci);
                // MOVSS/MOVSD XMM0, [RAX + RCX*{4,8} + HEADER_SIZE]. ModRM 0x44
                // (mod=01, reg=XMM0, r/m=SIB); SIB 0x88 (*4) / 0xC8 (*8), idx=RCX,
                // base=RAX; disp8 = HEADER_SIZE.
                let prefix = if is_d { 0xF2 } else { 0xF3 };
                let sib = if is_d { 0xC8 } else { 0x88 };
                self.buf
                    .emit(&[prefix, 0x0F, 0x10, 0x44, sib, HEADER_SIZE as u8]);
                self.fp_store(slot, XMM0, is_d);
            }
            // FP array element store (Slice B) — `fastore`/`dastore`. inputs =
            // [ctrl, mem, array, index, value]. Load the value into XMM0 first
            // (it must survive the guards; the bounds check clobbers only a GPR
            // scratch), then array→RAX, index→RCX, the null + bounds guards, and
            // `MOVSS`/`MOVSD` XMM0 into the element. Produces a memory token (no
            // result slot is read), but a slot is allocated for layout uniformity.
            Op::ArrayStore(kind) => {
                let _slot = self.alloc_slot(id);
                let is_d = matches!(kind, MemKind::Double);
                let bci = node.bytecode_pc.unwrap_or(0);
                self.fp_load(XMM0, self.slot_of(node.inputs[4]), is_d); // value → XMM0
                self.load_to_rax(self.slot_of(node.inputs[2])); // array → RAX
                self.load_to_rcx(self.slot_of(node.inputs[3])); // index → RCX
                self.emit_array_null_bounds_guards(bci);
                // MOVSS/MOVSD [RAX + RCX*{4,8} + HEADER_SIZE], XMM0 (opcode 0x11).
                let prefix = if is_d { 0xF2 } else { 0xF3 };
                let sib = if is_d { 0xC8 } else { 0x88 };
                self.buf
                    .emit(&[prefix, 0x0F, 0x11, 0x44, sib, HEADER_SIZE as u8]);
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
            Op::LambdaIntToDouble => {
                let slot = self.alloc_slot(id);
                self.load_reg_from_frame(CALL_ARG_REGS[0], self.context_slot_off);
                self.load_reg_from_frame(CALL_ARG_REGS[1], self.slot_of(node.inputs[2]));
                self.load_reg_from_frame(CALL_ARG_REGS[2], self.slot_of(node.inputs[3]));
                self.emit_mov_reg_imm64(RAX, self.lambda_int_to_double as u64);
                self.buf.emit(&[0xFF, 0xD0]);
                self.store_rax(slot);
            }
            Op::Call { info_ptr } => {
                let slot = self.alloc_slot(id);
                let num_args = node.inputs.len().saturating_sub(2);
                // fib44-fix follow-up: invoke_kind 4 marks a self-recursive call
                // the eligibility loop chose to emit as a DIRECT call to this
                // method's own entry (CRATONVM_JIT_IR_SELFREC_DIRECT), bypassing
                // the generic `jit_invoke_dispatch` helper. SAFETY: `info_ptr`
                // points to a live `JitInvokeInfo` owned by `ir_call_infos` for
                // the whole compile. `lower_data_node` has no post-`match` code,
                // so an early `return` here fully handles the node.
                if unsafe { (*(*info_ptr as *const JitInvokeInfo)).invoke_kind } == 4 {
                    self.emit_self_recursive_call(&node.inputs, slot, num_args);
                    return;
                }
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
                // 4. Exception sentinel. The dispatch helper returns `i64::MIN`
                //    when the callee threw/deopted. For an int/ref/void return
                //    that is unambiguous (no legitimate result is `i64::MIN`), so
                //    a plain `CMP RAX, i64::MIN; JE bail` suffices. For a `J`/`D`
                //    (long/double) return a legitimate `Long.MIN_VALUE` result is
                //    bit-identical to the sentinel, so on the (rare)
                //    `RAX == i64::MIN` branch we peek the out-of-band signal via
                //    `jit_dispatch_threw`: bail only when a genuine exception/
                //    deopt is pending, else keep the real value. (Only `J` is
                //    currently reachable — `static_call_shape` still rejects
                //    `D`/`F` returns until the XMM value tier.)
                self.emit_mov_reg_imm64(R10, i64::MIN as u64);
                self.buf.emit(&[0x4C, 0x39, 0xD0]); // CMP RAX, R10
                if matches!(node.ty, IrType::Long | IrType::Double | IrType::Float) {
                    // JNE .keep — common path: not the sentinel, keep real RAX.
                    self.buf.emit(&[0x0F, 0x85]);
                    let keep_patch = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    // Cold: RAX == i64::MIN. Peek whether a real exception/deopt
                    // is pending — MOV RAX, dispatch_threw ; CALL RAX (RAX = 0/1).
                    self.emit_mov_reg_imm64(RAX, self.dispatch_threw as u64);
                    self.buf.emit(&[0xFF, 0xD0]);
                    // TEST RAX, RAX — ZF=1 iff no signal pending (legit value).
                    self.buf.emit(&[0x48, 0x85, 0xC0]);
                    // Restore the sentinel/value into RAX before branching: the
                    // shared bail stub returns RAX unchanged (so it must be
                    // `i64::MIN`), and the keep path needs the genuine
                    // `Long.MIN_VALUE`. `MOV` does not disturb ZF.
                    self.emit_mov_reg_imm64(RAX, i64::MIN as u64);
                    // JNE bail_stub — ZF==0 ⇒ exception/deopt ⇒ propagate sentinel.
                    self.buf.emit(&[0x0F, 0x85]);
                    let patch = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    self.call_exc_patches.push(patch);
                    // .keep: patch the JNE above to land here.
                    let keep_off = self.buf.pos();
                    let rel = keep_off as i32 - (keep_patch as i32 + 4);
                    self.buf
                        .try_patch_i32(keep_patch, rel)
                        .expect("ir_lower call-sentinel keep patch in-bounds");
                } else {
                    self.buf.emit(&[0x0F, 0x84]); // JE rel32 (patched to the stub)
                    let patch = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    self.call_exc_patches.push(patch);
                }
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
        if self.schedule.blocks[block_idx]
            .successors
            .iter()
            .any(|&succ| succ <= block_idx)
        {
            self.emit_safepoint_poll();
        }
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
                        // splits the critical edges. Default layout:
                        //   TEST; JE around_true;
                        //   <true-edge phi copies>; JMP true_block;
                        //   around_true: <false-edge phi copies>; JMP false_block;
                        //
                        // wire-tiered-manager Step 4 (PGO handoff C1 → C2): pick
                        // the conditional-branch polarity from the profiled branch
                        // bias. `cmp` is nonzero exactly when the JVM branch is
                        // TAKEN (`successors[0]` = the taken edge), so the default
                        // layout makes the *taken* edge the fall-through — its
                        // forward `JE` is statically predicted not-taken. When the
                        // profile says this branch is usually NOT taken we invert
                        // to a `JNE` so the *not-taken* (false) edge becomes the
                        // fall-through instead. The two layouts are semantically
                        // identical — only the predicted/fall-through edge and the
                        // block order differ, and the phi copies stay attached to
                        // their own edge in both. With no hint for this PC (the
                        // default, and whenever profiling is off) `favor_false` is
                        // false and the emitted bytes are unchanged.
                        let favor_false = node
                            .bytecode_pc
                            .and_then(|pc| self.branch_hints.get(&pc).copied())
                            == Some(false);

                        // `(jcc, first_block, second_block)`: `jcc` skips the
                        // fall-through (`first_block`) to `second_block`.
                        //   default  (favor taken): JE  skips true→false; true first
                        //   inverted (favor !taken): JNE skips false→true; false first
                        let (jcc_second_byte, first_block, second_block) = if favor_false {
                            (0x85u8, false_block, true_block) // JNE
                        } else {
                            (0x84u8, true_block, false_block) // JE
                        };

                        // Jcc around_first (skip the fall-through edge).
                        self.buf.emit(&[0x0F, jcc_second_byte]);
                        let jcc_patch = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                        // Fall-through (favored) edge.
                        self.emit_phi_copies(block_idx, first_block);
                        self.buf.emit_byte(0xE9); // JMP first_block
                        let jmp_first = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                        self.branch_patches.push((jmp_first, first_block));

                        // around_first: the jumped-to edge. Patch the Jcc here.
                        let around_first = self.buf.pos();
                        let rel = around_first as i32 - (jcc_patch as i32 + 4);
                        self.buf
                            .try_patch_i32(jcc_patch, rel)
                            .expect("codegen patch in-bounds");
                        self.emit_phi_copies(block_idx, second_block);
                        self.buf.emit_byte(0xE9); // JMP second_block
                        let jmp_second = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                        self.branch_patches.push((jmp_second, second_block));
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
            // Float / double constant bits. A cat-2 `double` resolves to
            // `Double` (the resume builds a `Value::Double` with cat-2 two-slot
            // local placement); a cat-1 `float` resolves to `Float`.
            Op::ConstF(bits) => {
                if node.ty == IrType::Double {
                    FrameValue::Double(bits)
                } else {
                    FrameValue::Float(bits)
                }
            }
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
    /// (resolves to a `Value::Long` with cat-2 two-slot placement). A cat-1
    /// `Float` slot becomes `StackSlotFloat` (resolves to a `Value::Float` from
    /// the spilled 32-bit bits); a cat-2 `Double` slot becomes `StackSlotDouble`
    /// (resolves to a `Value::Double` with cat-2 two-slot placement). With
    /// FP-slot resume wired (Slice C), an FP value may now be live at a deopt
    /// guard. `Void`/`Control`/`Memory` are never live data slots, so they map to
    /// `Unsupported`.
    fn typed_stack_slot(off: i32, ty: IrType) -> FrameValue {
        match ty {
            IrType::Ref => FrameValue::StackSlotRef(off),
            IrType::Int => FrameValue::StackSlot(off),
            IrType::Long => FrameValue::StackSlotLong(off),
            IrType::Float => FrameValue::StackSlotFloat(off),
            IrType::Double => FrameValue::StackSlotDouble(off),
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
        // Continue (skip deopt) when the tested value is NON-zero — `JNZ` (the
        // near-Jcc second byte 0x85). Deopt when zero (ZF=1, JNZ not taken).
        self.emit_deopt_unless(0x85, bci, reason);
    }

    /// Emit a guard that deopts at `bci` UNLESS the just-set flags satisfy
    /// `jcc_continue` (the near-`Jcc` second byte, e.g. `0x85`=JNZ, `0x82`=JB).
    /// The "continue" condition falls through to the following code; otherwise
    /// control jumps to the shared deopt stub with this point's pointer in
    /// `DEOPT_ARG0`. Generalises `emit_deopt_if_zero` so a bounds check can
    /// continue on `JB` (unsigned index < length) and deopt otherwise.
    fn emit_deopt_unless(&mut self, jcc_continue: u8, bci: usize, reason: DeoptReason) {
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
        // J<continue> continue (condition holds → skip deopt).
        self.buf.emit(&[0x0F, jcc_continue]);
        let jcc_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        // Deopt path: load the point pointer into arg0, JMP to the shared stub.
        self.emit_mov_reg_imm64(DEOPT_ARG0, point_ptr);
        self.buf.emit_byte(0xE9);
        let jmp_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.deopt_stub_patches.push(jmp_patch);
        // continue:
        let cont = self.buf.pos();
        let rel = cont as i32 - (jcc_patch as i32 + 4);
        self.buf
            .try_patch_i32(jcc_patch, rel)
            .expect("deopt-unless Jcc patch in-bounds");
    }

    /// IR FP tier (Slice B) — emit the JVMS null + bounds deopt guards for an
    /// array element access, with the array pointer in RAX and the index in RCX
    /// (the layout the `MOVSS`/`MOVSD` SIB access below expects). On a null
    /// array or an out-of-bounds index, deopt at `bci`: the interpreter
    /// re-executes the array opcode and throws the exact NPE / AIOOBE with full
    /// semantics (including any in-method handler). Mirrors the single-pass
    /// inline checks, but routes the fault through the deopt path (which, with
    /// FP-slot resume — Slice C — reconstructs any live FP value precisely).
    /// Uses R10 as scratch for the length (the IR lowering never homes a value
    /// there). A non-faulting access continues with RAX/RCX unchanged.
    fn emit_array_null_bounds_guards(&mut self, bci: usize) {
        // Null check: TEST RAX,RAX → ZF=1 iff array == null. Continue on JNZ.
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        self.emit_deopt_if_zero(bci, DeoptReason::NullCheck);
        // Bounds check: MOV R10D, [RAX + ARRAY_LENGTH_OFFSET] (zero-extends to
        // R10), then CMP ECX, R10D. An UNSIGNED `index < length` (JB, CF=1)
        // continues; otherwise (index >= length, OR a negative index whose
        // unsigned value is huge) deopt → AIOOBE.
        self.buf
            .emit(&[0x44, 0x8B, 0x50, ARRAY_LENGTH_OFFSET as u8]); // MOV R10D,[RAX+12]
        self.buf.emit(&[0x44, 0x39, 0xD1]); // CMP ECX, R10D
        self.emit_deopt_unless(0x82, bci, DeoptReason::BoundsCheck); // JB continue
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
        // Guard-surviving scalar replacement (producer): when `sr_map` is set, a
        // snapshot slot holding a scalar-replaced (now-`Op::Dead`) `Op::New`
        // lowers to a `FrameValue::VirtualObject` (first occurrence) /
        // `VirtualObjectRef` (later occurrences), so a precise resume can
        // re-materialize the elided object instead of falling back to a
        // whole-method re-run. `emitted` tracks which objects already have their
        // defining `VirtualObject` in this frame. With `sr_map == None` this is
        // exactly the historical `frame_value_for` mapping (byte-identical).
        if let Some(sr) = self.sr_map {
            let deopt_block = self.deopt_block_for_bci(sp.bci);
            if std::env::var_os("CRATONVM_DBG_SCALAR_DEOPT").is_some() {
                let matches: Vec<NodeId> = sp
                    .locals
                    .iter()
                    .chain(sp.stack.iter())
                    .copied()
                    .filter(|n| *n != NO_NODE && sr.objects.contains_key(n))
                    .collect();
                eprintln!(
                    "[DBG_SCALAR_DEOPT] resolve bci {} deopt_block={:?} sr_objects={} matching_slots={:?}",
                    sp.bci,
                    deopt_block,
                    sr.objects.len(),
                    matches
                );
            }
            let mut emitted: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
            let mut locals = Vec::with_capacity(sp.locals.len());
            for &n in &sp.locals {
                locals.push(if n != NO_NODE && sr.objects.contains_key(&n) {
                    self.frame_value_for_object(n, deopt_block, sr, &mut emitted)
                } else {
                    self.frame_value_for(n)
                });
            }
            let mut stack = Vec::with_capacity(sp.stack.len());
            for &n in &sp.stack {
                stack.push(if n != NO_NODE && sr.objects.contains_key(&n) {
                    self.frame_value_for_object(n, deopt_block, sr, &mut emitted)
                } else {
                    self.frame_value_for(n)
                });
            }
            return FrameState {
                method_key: String::new(),
                bci: sp.bci as u32,
                locals,
                stack,
                monitors: Vec::new(),
                caller: None,
            };
        }
        FrameState {
            method_key: String::new(),
            bci: sp.bci as u32,
            locals: sp.locals.iter().map(|&n| self.frame_value_for(n)).collect(),
            stack: sp.stack.iter().map(|&n| self.frame_value_for(n)).collect(),
            monitors: Vec::new(),
            caller: None,
        }
    }

    /// Block where the deopt at `bci` fires — the program point all of a
    /// scalar-replaced object's field stores must dominate for its
    /// `VirtualObject` emission to be temporally correct. v1 deopt points are
    /// div/rem guards, so the block is that of the `Op::Div`/`Op::Rem` node
    /// carrying this bci. Returns `None` (⇒ the producer bails to `Undefined`)
    /// when the block can't be uniquely identified.
    fn deopt_block_for_bci(&self, bci: usize) -> Option<usize> {
        let mut found: Option<usize> = None;
        for (id, n) in self.graph.nodes.iter().enumerate() {
            // The deopt at `bci` fires from a div/rem zero/overflow guard (whose
            // node carries `bytecode_pc == bci`) or an explicit `Op::Guard { bci }`.
            let is_deopt_here = match &n.op {
                Op::Div | Op::Rem => n.bytecode_pc == Some(bci),
                Op::Guard { bci: gb } => *gb == bci,
                _ => false,
            };
            if is_deopt_here {
                let b = *self.schedule.node_to_block.get(id)?;
                if b == usize::MAX {
                    return None;
                }
                match found {
                    Some(prev) if prev != b => return None, // ambiguous
                    _ => found = Some(b),
                }
            }
        }
        found
    }

    /// Lower a scalar-replaced object (`new_id`, an eliminated `Op::New`) that is
    /// live in a deopt snapshot slot into a `FrameValue::VirtualObject` (its
    /// first occurrence in this frame) or `VirtualObjectRef` (a later, shared
    /// occurrence). Bails to `FrameValue::Undefined` (⇒ safe whole-method re-run)
    /// unless every soundness condition holds:
    ///   * a deopt block is known, and the `Op::New` + every eliminated field
    ///     store **strictly dominate** it — so each field genuinely holds its
    ///     recorded value at the deopt bci (a deopt *before* a store would
    ///     otherwise materialize a post-store value);
    ///   * no field value is itself another scalar-replaced (virtual) object —
    ///     nested virtual graphs are a deferred follow-up (v1);
    ///   * every field value resolves to a real machine/const `FrameValue`
    ///     (never `Undefined`/`Unsupported`), which `resolve_value` makes
    ///     concrete from machine state at deopt time.
    fn frame_value_for_object(
        &self,
        new_id: NodeId,
        deopt_block: Option<usize>,
        sr: &ScalarReplacementMap,
        emitted: &mut std::collections::HashSet<NodeId>,
    ) -> FrameValue {
        let dbg = std::env::var_os("CRATONVM_DBG_SCALAR_DEOPT").is_some();
        let info = match sr.objects.get(&new_id) {
            Some(i) => i,
            None => return FrameValue::Undefined,
        };
        let db = match deopt_block {
            Some(b) => b,
            None => {
                if dbg {
                    eprintln!("[DBG_SCALAR_DEOPT] bail new {new_id}: no deopt block for bci");
                }
                return FrameValue::Undefined;
            }
        };
        // The allocation and every field store must have executed before the
        // deopt (strict block dominance — same-block ordering is conservatively
        // rejected; see `Schedule::node_strictly_dominates_block`). We test the
        // *control* node of each — the New/store nodes themselves are now
        // `Op::Dead` (unscheduled), but their captured control inputs are live
        // and carry the same block.
        if !self
            .schedule
            .node_strictly_dominates_block(info.new_ctrl, db)
        {
            if dbg {
                eprintln!(
                    "[DBG_SCALAR_DEOPT] bail new {new_id}: new_ctrl {} (block {:?}) !strict-dom deopt block {db}",
                    info.new_ctrl,
                    self.schedule.node_to_block.get(info.new_ctrl as usize)
                );
            }
            return FrameValue::Undefined;
        }
        for &store_ctrl in &info.store_ctrls {
            if !self.schedule.node_strictly_dominates_block(store_ctrl, db) {
                if dbg {
                    eprintln!(
                        "[DBG_SCALAR_DEOPT] bail new {new_id}: store_ctrl {} (block {:?}) !strict-dom deopt block {db}",
                        store_ctrl,
                        self.schedule.node_to_block.get(store_ctrl as usize)
                    );
                }
                return FrameValue::Undefined;
            }
        }
        // A later occurrence of an already-defined object is a back/shared edge.
        if emitted.contains(&new_id) {
            return FrameValue::VirtualObjectRef(new_id as usize);
        }
        // Build per-field values. `None` ⇒ zero default (admitted allocations set
        // no non-zero primitive field in <init>). A field whose value is itself a
        // scalar-replaced New (nested virtual) or an unresolvable slot bails the
        // whole object.
        let mut field_values: Vec<FrameValue> = Vec::with_capacity(info.num_fields);
        for i in 0..info.num_fields {
            let fv = match info.field_values.get(i).copied().flatten() {
                None => FrameValue::Int(0),
                Some(vnode) => {
                    if sr.objects.contains_key(&vnode) {
                        if dbg {
                            eprintln!("[DBG_SCALAR_DEOPT] bail new {new_id}: field {i} is nested virtual (node {vnode})");
                        }
                        return FrameValue::Undefined; // nested virtual — deferred
                    }
                    let fv = self.frame_value_for(vnode);
                    if matches!(fv, FrameValue::Undefined | FrameValue::Unsupported) {
                        if dbg {
                            eprintln!("[DBG_SCALAR_DEOPT] bail new {new_id}: field {i} node {vnode} -> {fv:?}");
                        }
                        return FrameValue::Undefined;
                    }
                    fv
                }
            };
            field_values.push(fv);
        }
        emitted.insert(new_id);
        if std::env::var_os("CRATONVM_DBG_SCALAR_DEOPT").is_some() {
            eprintln!(
                "[DBG_SCALAR_DEOPT] emit VirtualObject (new {new_id}, class_id {}, {} field(s)) at deopt block {db}",
                info.class_id, info.num_fields
            );
        }
        FrameValue::VirtualObject(VirtualObjectState {
            id: new_id as usize,
            class_id: info.class_id,
            num_fields: info.num_fields,
            field_values,
        })
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
    // No profile → empty branch hints; no scalar-deopt → None. Both default to
    // the historical byte-for-byte layout. An empty `HashMap` performs no
    // allocation until first insert.
    let empty: HashMap<usize, bool> = HashMap::new();
    lower_inner(
        graph, schedule, num_params, num_locals, helpers, &empty, None,
    )
}

/// `lower` with profile-guided conditional-branch layout (wire-tiered-manager
/// Step 4 — PGO handoff C1 → C2). `branch_hints` maps a conditional-branch
/// instruction's bytecode PC to its bias (`true` = usually taken, `false` =
/// usually not taken); see `Lowerer::branch_hints`. An empty map reproduces
/// [`lower`] exactly.
pub fn lower_with_branch_hints(
    graph: &Graph,
    schedule: &Schedule,
    num_params: usize,
    num_locals: usize,
    helpers: &JitRuntimeHelpers,
    branch_hints: &HashMap<usize, bool>,
) -> Option<CompiledMethod> {
    lower_inner(
        graph,
        schedule,
        num_params,
        num_locals,
        helpers,
        branch_hints,
        None,
    )
}

/// As [`lower`], but with an optional [`ScalarReplacementMap`] enabling the
/// guard-surviving scalar-replacement deopt producer (a deopt slot holding a
/// scalar-replaced `Op::New` lowers to a `FrameValue::VirtualObject`). The
/// production caller passes `Some(map)` only when `CRATONVM_SCALAR_DEOPT` and
/// `CRATONVM_DEOPT_REAL` are both set; `None` is byte-identical to the prior
/// `lower`.
pub fn lower_with_scalar_deopt(
    graph: &Graph,
    schedule: &Schedule,
    num_params: usize,
    num_locals: usize,
    helpers: &JitRuntimeHelpers,
    sr_map: Option<&ScalarReplacementMap>,
) -> Option<CompiledMethod> {
    let empty: HashMap<usize, bool> = HashMap::new();
    lower_inner(
        graph, schedule, num_params, num_locals, helpers, &empty, sr_map,
    )
}

/// Shared lowering body: both profile-guided branch hints and the optional
/// guard-surviving scalar-replacement map flow in here. `pub(crate)` so the
/// production compile path (`lib.rs`) can supply BOTH at once.
pub(crate) fn lower_inner(
    graph: &Graph,
    schedule: &Schedule,
    num_params: usize,
    num_locals: usize,
    helpers: &JitRuntimeHelpers,
    branch_hints: &HashMap<usize, bool>,
    sr_map: Option<&ScalarReplacementMap>,
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
        branch_hints,
        sr_map,
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
    lowerer.emit_safepoint_poll();

    // Emit blocks in order
    for block_idx in 0..schedule.blocks.len() {
        lowerer.lower_block(block_idx);
    }

    // Soundness bail (ES SortingDigestTests -Jit on): some emitted use asked
    // for the frame slot of a node that was never allocated one (scheduler
    // gap — the node sits in no emitted block). The emitted code would read
    // `[rbp - 0]`, i.e. the saved caller RBP, as a data value. Discard the
    // artifact and let the caller fall back to the single-pass backend.
    if lowerer.unallocated_slot_use.get() {
        return None;
    }

    // real-frame-deopt (step 3): emit the shared deopt stub after the method
    // body so failed guards can jump to it, then patch in-method branches.
    lowerer.emit_deopt_stub();
    // Gap B: emit the shared call-exception bail stub after the body so each
    // dispatch site's sentinel `JE` reaches it.
    lowerer.emit_call_exc_stub();

    lowerer.patch_branches();
    // fib44-fix follow-up: patch direct self-recursive calls to the method entry.
    lowerer.patch_self_calls();

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
    // Guard-surviving scalar replacement: if any deopt frame carries a
    // scalar-replaced object as a `FrameValue::VirtualObject`, route this method
    // through the interpreter's precise-resume + materialize path
    // (`resume_real_ir_deopt` → `materialize_virtual_objects`) instead of the
    // no-materialize int-only path / whole-method re-run. Without this the
    // `can_deopt_resume` gate stays off for the IR backend and the emitted
    // VirtualObject is never consumed. Sound to enable: the per-slot mapper and
    // the materializer each bail to a safe whole-method re-run on any slot they
    // cannot reconstruct. Only reachable with `sr_map` set (i.e.
    // `CRATONVM_SCALAR_DEOPT` + `CRATONVM_DEOPT_REAL`), so production is unaffected.
    if sr_map.is_some()
        && cm
            ._deopt_point_boxes
            .iter()
            .any(|p| crate::deopt::count_virtual_objects(&p.frame_state) > 0)
    {
        cm.can_deopt_resume = true;
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

    #[test]
    fn cooperative_poll_runs_in_a_pure_ir_method() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static FLAG: u8 = 1;
        static HITS: AtomicUsize = AtomicUsize::new(0);
        extern "C" fn slow_poll() {
            HITS.fetch_add(1, Ordering::SeqCst);
        }

        let code = [0x04, 0xac]; // iconst_1; ireturn
        let builder = IrBuilder::new(0, 0);
        let graph = builder.build(&code, code.len()).expect("IR build");
        let schedule = ir_schedule::schedule(&graph);
        let mut helpers = no_helpers();
        helpers.safepoint_flag_addr = &FLAG as *const u8 as usize;
        helpers.safepoint_slow_path = slow_poll as *const () as usize;
        HITS.store(0, Ordering::SeqCst);
        let compiled =
            lower(&graph, &schedule, 0, 0, &helpers).expect("pure IR method should lower");

        // SAFETY: the generated function has no arguments and returns int 1.
        assert_eq!(unsafe { compiled.try_call(&[]) }, Ok(1));
        assert_eq!(HITS.load(Ordering::SeqCst), 1);
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

    /// True iff `needle` appears as a contiguous subsequence of `hay`.
    fn contains_seq(hay: &[u8], needle: &[u8]) -> bool {
        needle.len() <= hay.len() && hay.windows(needle.len()).any(|w| w == needle)
    }

    // ── wire-tiered-manager Step 4: PGO branch-bias in the IR (C2) path ──

    /// The optimizing IR lowerer must consume the profiled branch bias: a
    /// conditional the profile marks "usually NOT taken" flips from `JE`
    /// (`0F 84`) to `JNE` (`0F 85`) so the not-taken edge becomes the
    /// fall-through. No hint (the default) ⇒ the historical `JE` layout, and
    /// a "usually taken" hint reproduces it byte-for-byte — only the
    /// not-taken case inverts.
    #[test]
    fn step4_ir_lower_consumes_branch_bias_hint() {
        // iload_0; ifeq +5 (→pc6); iconst_1; ireturn; iconst_0; ireturn.
        // One conditional branch (the `ifeq` at pc 1) with two successor
        // edges, no phis, no calls, no guards → the ONLY Jcc in the emitted
        // body is the `Op::If` terminator.
        let code = [0x1a, 0x99, 0x00, 0x05, 0x04, 0xac, 0x03, 0xac];
        let code_len = 8;

        let build = || {
            let builder = IrBuilder::new(1, 1);
            let mut graph = builder.build(&code, code_len).expect("IR build");
            ir_optimize::optimize(&mut graph);
            let schedule = ir_schedule::schedule(&graph);
            (graph, schedule)
        };

        // Default (no hint): a `JE` (0F 84), no inverted form.
        let (g0, s0) = build();
        let base_code = lower(&g0, &s0, 1, 1, &no_helpers())
            .expect("lower baseline")
            .code_bytes()
            .to_vec();
        assert!(
            contains_seq(&base_code, &[0x0F, 0x84]),
            "baseline IR branch should emit JE (0F 84)"
        );

        // "usually not taken" at the ifeq PC (1): inverted to `JNE` (0F 85),
        // and a different code buffer.
        let mut hints = HashMap::new();
        hints.insert(1usize, false);
        let (g1, s1) = build();
        let hint_code = lower_with_branch_hints(&g1, &s1, 1, 1, &no_helpers(), &hints)
            .expect("lower hinted")
            .code_bytes()
            .to_vec();
        assert!(
            contains_seq(&hint_code, &[0x0F, 0x85]),
            "usually-not-taken hint should invert the IR branch to JNE (0F 85)"
        );
        assert_ne!(
            base_code, hint_code,
            "branch-bias hint must change the emitted code"
        );

        // "usually taken" keeps the default JE layout (byte-identical).
        let mut taken_hints = HashMap::new();
        taken_hints.insert(1usize, true);
        let (g2, s2) = build();
        let taken_code = lower_with_branch_hints(&g2, &s2, 1, 1, &no_helpers(), &taken_hints)
            .expect("lower taken-hinted")
            .code_bytes()
            .to_vec();
        assert_eq!(
            base_code, taken_code,
            "usually-taken hint must reproduce the default JE layout byte-for-byte"
        );
    }

    /// A hint for an UNRELATED bytecode PC must not perturb codegen — only the
    /// branch whose own PC is marked not-taken inverts.
    #[test]
    fn step4_ir_lower_branch_bias_keyed_by_pc() {
        let code = [0x1a, 0x99, 0x00, 0x05, 0x04, 0xac, 0x03, 0xac];
        let build = || {
            let builder = IrBuilder::new(1, 1);
            let mut graph = builder.build(&code, 8).expect("IR build");
            ir_optimize::optimize(&mut graph);
            let schedule = ir_schedule::schedule(&graph);
            (graph, schedule)
        };
        let (g0, s0) = build();
        let base = lower(&g0, &s0, 1, 1, &no_helpers())
            .expect("lower")
            .code_bytes()
            .to_vec();
        // Hint a PC that does not correspond to this method's branch (7).
        let mut hints = HashMap::new();
        hints.insert(7usize, false);
        let (g1, s1) = build();
        let other = lower_with_branch_hints(&g1, &s1, 1, 1, &no_helpers(), &hints)
            .expect("lower")
            .code_bytes()
            .to_vec();
        assert_eq!(
            base, other,
            "a hint for an unrelated PC must not change codegen"
        );
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

    // ── Guard-surviving scalar replacement (producer) ────────────────────

    /// Build a 3-block graph modelling a scalar-replaced object live at a guard:
    ///
    /// ```text
    /// block0 (entry):  o = new Foo(); o.x = 7; if (cond) ...   (New + store here)
    /// block1 (taken):  guard(cond != 0) [bci 10]; return o.x   (deopt point here)
    /// block2 (else):   return 0
    /// ```
    ///
    /// `same_block_guard` puts the guard in block0 instead (no `If`), so the New/
    /// store do NOT strictly dominate it — the temporal-hazard bail case.
    /// `dup_local` puts the object in TWO local slots (sharing → `VirtualObjectRef`).
    /// Returns `(graph, sr_map, new_id)`; the New + store are marked `Op::Dead`
    /// (simulating `apply_ea_to_ir`) and `sr_map` captures their control inputs.
    fn build_sr_deopt_graph(
        same_block_guard: bool,
        dup_local: bool,
    ) -> (Graph, ScalarReplacementMap, NodeId) {
        use crate::ir::CmpOp;
        let mut g = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
        };
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let cond = g.add(Op::Param(0), IrType::Int, vec![start], None);
        // o = new Foo(); o.x = 7  (both controlled by c0 → block0)
        let newo = g.add(
            Op::New {
                class_id: 7,
                num_fields: 1,
            },
            IrType::Ref,
            vec![c0, mem],
            None,
        );
        let f0 = g.add(Op::Const(0), IrType::Int, vec![], None); // field index 0
        let v7 = g.add(Op::Const(7), IrType::Int, vec![], None); // field value
        let store = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![c0, mem, newo, f0, v7],
            None,
        );
        let new_ctrl = c0;
        let store_ctrl = c0;

        // Locals at the guard's safepoint: [cond, o] (or [cond, o, o] for sharing).
        let locals = if dup_local {
            vec![cond, newo, newo]
        } else {
            vec![cond, newo]
        };

        let guard_ctrl = if same_block_guard {
            // Guard in block0 (after the store), no branch → same block as New/store.
            let guard = g.add(Op::Guard { bci: 10 }, IrType::Void, vec![c0, cond], None);
            let _ = guard;
            let ret = g.add(Op::Return, IrType::Void, vec![c0, v7], None);
            g.exit = ret;
            c0
        } else {
            // if (cond) → block1 (taken) / block2 (else); guard in block1.
            let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
            let cmp = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![cond, zero], None);
            let iff = g.add(Op::If, IrType::Control, vec![c0, cmp], None);
            let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
            let e = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
            let guard = g.add(Op::Guard { bci: 10 }, IrType::Void, vec![t, cond], None);
            let _ = guard;
            let ret1 = g.add(Op::Return, IrType::Void, vec![t, v7], None);
            let ret2 = g.add(Op::Return, IrType::Void, vec![e, v7], None);
            g.exit = ret1;
            let _ = ret2;
            t
        };
        let _ = guard_ctrl;

        g.safepoints.push(SafepointSnapshot {
            bci: 10,
            locals,
            stack: vec![],
        });

        // Simulate `apply_ea_to_ir`: mark the New + store dead (their inputs are
        // cleared), exactly as the production path does before scheduling.
        g.nodes[newo as usize].op = Op::Dead;
        g.nodes[newo as usize].inputs.clear();
        g.nodes[store as usize].op = Op::Dead;
        g.nodes[store as usize].inputs.clear();

        let mut objects = HashMap::new();
        objects.insert(
            newo,
            VirtualObjectInfo {
                class_id: 7,
                num_fields: 1,
                field_values: vec![Some(v7)],
                new_ctrl,
                store_ctrls: vec![store_ctrl],
            },
        );
        (g, ScalarReplacementMap { objects }, newo)
    }

    /// Find the deopt point at `bci` among a lowered method's baked guard boxes
    /// (`_deopt_point_boxes` — the runtime-used frame states; an `Op::Guard`
    /// emits its box there via `emit_deopt_unless`, NOT into the `bci_native`-keyed
    /// `deopt_points` list).
    fn deopt_locals_at(cm: &CompiledMethod, bci: u32) -> Vec<FrameValue> {
        cm._deopt_point_boxes
            .iter()
            .find(|p| p.bci == bci)
            .unwrap_or_else(|| panic!("no deopt box at bci {bci}"))
            .frame_state
            .locals
            .clone()
    }

    #[test]
    fn test_scalar_deopt_emits_virtual_object() {
        // A scalar-replaced object whose New + (constant) field store strictly
        // dominate the guard's block lowers to a `VirtualObject` with the stored
        // value as its field — the guard-surviving case.
        let (g, sr_map, newo) = build_sr_deopt_graph(false, false);
        let schedule = ir_schedule::schedule(&g);
        let cm = lower_with_scalar_deopt(&g, &schedule, 1, 3, &no_helpers(), Some(&sr_map))
            .expect("lower");
        let locals = deopt_locals_at(&cm, 10);
        // local[1] is the object → VirtualObject{ class_id:7, field_values:[Int(7)] }.
        match &locals[1] {
            FrameValue::VirtualObject(state) => {
                assert_eq!(state.id, newo as usize);
                assert_eq!(state.class_id, 7);
                assert_eq!(state.num_fields, 1);
                assert_eq!(state.field_values, vec![FrameValue::Int(7)]);
            }
            other => panic!("expected VirtualObject, got {other:?}"),
        }
    }

    #[test]
    fn test_scalar_deopt_bails_when_store_not_dominating() {
        // Guard in the SAME block as the New/store → strict dominance fails (v1
        // conservatively rejects same-block ordering) → bail to Undefined → the
        // resume falls back to a safe whole-method re-run.
        let (g, sr_map, _newo) = build_sr_deopt_graph(true, false);
        let schedule = ir_schedule::schedule(&g);
        let cm = lower_with_scalar_deopt(&g, &schedule, 1, 3, &no_helpers(), Some(&sr_map))
            .expect("lower");
        let locals = deopt_locals_at(&cm, 10);
        assert_eq!(
            locals[1],
            FrameValue::Undefined,
            "same-block store must bail to Undefined (safe re-run)"
        );
    }

    #[test]
    fn test_scalar_deopt_shares_via_ref() {
        // The same object in two local slots: first occurrence defines the
        // VirtualObject, the second is a VirtualObjectRef to its id.
        let (g, sr_map, newo) = build_sr_deopt_graph(false, true);
        let schedule = ir_schedule::schedule(&g);
        let cm = lower_with_scalar_deopt(&g, &schedule, 1, 3, &no_helpers(), Some(&sr_map))
            .expect("lower");
        let locals = deopt_locals_at(&cm, 10);
        assert!(
            matches!(&locals[1], FrameValue::VirtualObject(s) if s.id == newo as usize),
            "first occurrence defines the object, got {:?}",
            locals[1]
        );
        assert_eq!(
            locals[2],
            FrameValue::VirtualObjectRef(newo as usize),
            "second occurrence is a ref to the same id"
        );
    }

    #[test]
    fn test_scalar_deopt_disabled_without_map() {
        // With `sr_map = None` (the default), the dead-New slot resolves to
        // Undefined exactly as before — byte-identical to the prior producer.
        let (g, _sr_map, _newo) = build_sr_deopt_graph(false, false);
        let schedule = ir_schedule::schedule(&g);
        let cm = lower(&g, &schedule, 1, 3, &no_helpers()).expect("lower");
        let locals = deopt_locals_at(&cm, 10);
        assert_eq!(locals[1], FrameValue::Undefined);
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
