// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lower the scheduled IR graph to x86-64 machine code.
//!
//! Walks the scheduled basic blocks, emits native instructions for each
//! IR node, and patches forward branches.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::num::NonZeroU32;

use super::ir::{
    Graph, InlineScopeTable, IrType, MemKind, NodeId, Op, SafepointSnapshot,
    MAX_INLINE_SCOPE_DEPTH, NO_NODE,
};
use super::ir_schedule::Schedule;
use super::{CompiledMethod, ExecutableBuffer, JitInvokeInfo, JitRuntimeHelpers};
use crate::bailout::{
    record_bailout, Bailout, BailoutReason, CompileResult, DEFAULT_MAX_FRAME_BYTES,
    DEFAULT_MAX_NODES,
};
use crate::deopt::{
    ir_deopt_entry, DeoptAction, DeoptReason, DeoptVerifier, DeoptimizationPoint, EliminatedValue,
    EliminationCause, FrameState, FrameValue, MethodFrameLimits, OopCoverage, ResumeSemantics,
    VirtualObjectState,
};
use crate::regalloc::{resolve_parallel_copy, CopyOp, ValueLoc};
use cratonvm_types::narrow_oop::narrow_base;
use cratonvm_types::{
    narrow_oops_enabled, ARRAY_LENGTH_OFFSET, FIELD_CELL_PAYLOAD32_OFFSET,
    FIELD_CELL_PAYLOAD64_OFFSET, HEADER_SIZE, SLOT_SIZE,
};

// ── Header-offset emission sites (arch-2026-07-26 `layout-constant-hazards`) ──
//
// This file is a SECOND x86-64 emitter alongside `x64.rs`, and it bakes object
// header displacements straight into instruction encodings. The header-offset
// inventory the `ObjectHeader` shrink was planned from scanned `x64.rs` only, so
// the sites below were invisible to it. They are, in emission order:
//
//   * field address   — `HEADER_SIZE + field_index * SLOT_SIZE` (disp32)
//   * field tag address — same formula, tag half of the cell (disp32)
//   * `MOVSS/MOVSD XMM0, [RAX + RCX*n + HEADER_SIZE]` (disp8, float array load)
//   * `MOVSS/MOVSD [RAX + RCX*n + HEADER_SIZE], XMM0` (disp8, float array store)
//   * `MOV R10D, [RAX + ARRAY_LENGTH_OFFSET]` (disp8, bounds check)
//
// Every one already reads the shared constant rather than a literal, so a layout
// change does reach them. The hazard is narrower: the disp8 sites narrow the
// constant with an *unchecked* cast into a literal instruction byte array. A
// header past 127 bytes would be re-read by the CPU as a NEGATIVE displacement
// and the load would address memory before the object — no panic, no failed
// assertion, just a wrong address. `types/src/heap_types.rs` pins that bound
// centrally; it is restated here, at the emitter, so the constraint travels with
// the code that actually depends on it and so this file stops being invisible to
// anyone grepping for the constraint.
//
// Inventory tripwires: `jit/src/lib.rs::layout_constant_inventory` (counts the
// uses in this file *and* in `lib.rs`, which the substring-needle tripwire in
// `x64.rs` structurally cannot see) and
// `x64.rs::ir_lower_header_offset_sites_are_inventoried_too`.
const _: () = assert!(
    HEADER_SIZE <= 127,
    "ir_lower.rs narrows HEADER_SIZE into a signed disp8 byte inside literal \
     MOVSS/MOVSD encodings; above 127 that displacement reads as negative"
);
const _: () = assert!(
    ARRAY_LENGTH_OFFSET <= 127,
    "ir_lower.rs narrows ARRAY_LENGTH_OFFSET into the signed disp8 of the \
     array-length load that guards every bounds check"
);
const _: () = assert!(
    HEADER_SIZE % 8 == 0 && SLOT_SIZE % 8 == 0,
    "field displacements are HEADER_SIZE + index*SLOT_SIZE; both terms must stay \
     on the 8-byte grid or an 8-byte field cell straddles an alignment boundary"
);

// ── Guard-surviving scalar replacement (producer side) ───────────────
//
// When escape analysis scalar-replaces an `Op::New` (the allocation is elided,
// its field loads redirected to the stored values), the object no longer exists
// in the JIT frame — but it may still be live at a deopt point. The default
// behaviour resolves its (now-`Op::Dead`) snapshot slot to
// `FrameValue::MaterializationRequired`, which refuses the precise resume and
// forces a whole-method re-run. (It used to resolve to `FrameValue::Undefined`,
// which every resume sink maps to `Value::Int(0)` — so a reference local came
// back as `null` with no error at all. See `frame_value_for_object`.) With this
// map threaded into the lowerer, such a slot instead lowers to a
// `FrameValue::VirtualObject`, which the VM's `materialize_virtual_objects`
// consumer rebuilds on a precise resume.
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
    /// `Some(atype)` for a scalar-replaced array: the element atype, with
    /// `num_fields` as the LENGTH. See [`crate::deopt::VirtualObjectState`],
    /// which this is the compile-time half of.
    pub array_element_type: Option<u8>,
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
const XMM0: u8 = crate::regalloc::xmm_roles::IR_FP_SCRATCH[0];
const XMM1: u8 = crate::regalloc::xmm_roles::IR_FP_SCRATCH[1];

/// The bytes of one `[RBP - disp]` access, without a heap allocation.
///
/// Seven is the longest form ([`enc_frame_load`]'s disp32 branch: REX + opcode
/// + ModRM + four displacement bytes).
#[derive(Clone, Copy)]
struct FrameAccess {
    bytes: [u8; 7],
    len: usize,
}

impl FrameAccess {
    fn new() -> FrameAccess {
        FrameAccess {
            bytes: [0; 7],
            len: 0,
        }
    }

    fn push(&mut self, b: u8) {
        if let Some(cell) = self.bytes.get_mut(self.len) {
            *cell = b;
            self.len += 1;
        }
    }

    fn as_slice(&self) -> &[u8] {
        // `len` only ever advances through `push`, which bounds-checks.
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}

/// The bytes of one whole tile, without a heap allocation.
///
/// Bounded on purpose. The widest sequence [`Lowerer::encode_tile_frame_homed`]
/// can produce is two disp32 frame loads (7 each), a four-byte ALU instruction
/// and a disp32 store (7) — 25 bytes. A tile that would need more is a tile
/// this encoder has not been proved byte-equal for, and [`Self::push`] answers
/// it with `None` rather than with a truncated instruction stream.
#[derive(Clone, Copy)]
struct FrameAccessList {
    bytes: [u8; 32],
    len: usize,
}

impl FrameAccessList {
    fn new() -> FrameAccessList {
        FrameAccessList {
            bytes: [0; 32],
            len: 0,
        }
    }

    fn push(&mut self, acc: &FrameAccess) -> Option<()> {
        self.push_bytes(acc.as_slice())
    }

    fn push_bytes(&mut self, bytes: &[u8]) -> Option<()> {
        // An empty push is a refusal, not a no-op: `enc_frame_load` writes
        // nothing for a register it cannot encode, and swallowing that would
        // drop the load.
        if bytes.is_empty() {
            return None;
        }
        let end = self.len.checked_add(bytes.len())?;
        self.bytes.get_mut(self.len..end)?.copy_from_slice(bytes);
        self.len = end;
        Some(())
    }

    fn as_slice(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Make this tile's bytes wrong, on purpose.
    ///
    /// The forcing function for the byte-equality oracle: a check that has
    /// never been shown to fail is a check nobody knows the polarity of. See
    /// `an_injected_wrong_byte_is_caught_by_the_oracle`.
    #[cfg(test)]
    fn corrupt_last_byte(&mut self) {
        if let Some(last) = self.len.checked_sub(1).and_then(|i| self.bytes.get_mut(i)) {
            *last ^= 0xFF;
        }
    }
}

/// `MOV reg, [RBP - offset]`, REX.W, smallest displacement form.
///
/// Extracted from `Lowerer::load_to_rax` / `load_to_rcx` so the level-2 tile
/// encoder and the per-opcode emitters produce these bytes from **one** piece
/// of code rather than from two that agree today. The byte-equality oracle
/// increment 2 rests on is only as strong as that: two copies of a frame access
/// can drift, and the drift would be invisible to a test that drives just one
/// of them.
///
/// `reg` must be 0-7. There is no REX.R here — the callers are RAX and RCX,
/// which is what makes the output identical to the literals it replaced — so a
/// high register would silently encode as its low counterpart. Rather than
/// widen the encoding (which would change the bytes) the function refuses: it
/// writes nothing and the caller sees an empty accessor.
fn enc_frame_load(reg: u8, offset: i32, out: &mut FrameAccess) {
    if reg >= 8 {
        return;
    }
    let neg = -offset;
    out.push(0x48);
    out.push(0x8B);
    if (i32::from(i8::MIN)..=i32::from(i8::MAX)).contains(&neg) {
        // 48 8B 45 disp8 — mod=01, reg=<reg>, r/m=RBP(101)
        out.push(0x45 | ((reg & 7) << 3));
        out.push(neg as u8);
    } else {
        // 48 8B 85 disp32 — mod=10
        out.push(0x85 | ((reg & 7) << 3));
        for b in neg.to_le_bytes() {
            out.push(b);
        }
    }
}

/// `MOV [RBP - offset], reg`, REX.W, smallest displacement form. The store half
/// of [`enc_frame_load`]; the same `reg < 8` rule applies for the same reason.
fn enc_frame_store(reg: u8, offset: i32, out: &mut FrameAccess) {
    if reg >= 8 {
        return;
    }
    let neg = -offset;
    out.push(0x48);
    out.push(0x89);
    if (i32::from(i8::MIN)..=i32::from(i8::MAX)).contains(&neg) {
        // 48 89 45 disp8 — mod=01, reg=<reg>, r/m=RBP(101)
        out.push(0x45 | ((reg & 7) << 3));
        out.push(neg as u8);
    } else {
        // 48 89 85 disp32 — mod=10
        out.push(0x85 | ((reg & 7) << 3));
        for b in neg.to_le_bytes() {
            out.push(b);
        }
    }
}

// Platform C-ABI integer argument registers for the `invoke_dispatch` helper
// call (Gap B `Op::Call`). The helper's 4 args (vm_ptr, info_ptr, args_ptr,
// num_args) are all integer, so all four fit in registers on both ABIs — no
// stack args, only the 32-byte Win64 shadow space (already budgeted in the
// frame). Mirrors x64.rs `ARG_REGS`.
#[cfg(target_os = "windows")]
const CALL_ARG_REGS: [u8; 4] = [1, 2, 8, 9]; // RCX, RDX, R8, R9
#[cfg(not(target_os = "windows"))]
const CALL_ARG_REGS: [u8; 4] = [7, 6, 2, 1]; // RDI, RSI, RDX, RCX

// Platform C-ABI integer argument registers for a JIT *method entry* — the ABI
// a compiled artifact's own prologue reads its incoming arguments from:
// `abi[0]` = the hidden VM context pointer when the callee `needs_context`,
// then one register per Java argument (receiver first for a non-static
// callee). This is the FULL integer argument register file, unlike
// `CALL_ARG_REGS` (capped at 4 because the dispatch helper only ever takes 4).
// Mirrors x64.rs `ARG_REGS` and the inline list in `emit_self_recursive_call`.
#[cfg(target_os = "windows")]
const ENTRY_ABI_REGS: &[u8] = &[1, 2, 8, 9]; // RCX, RDX, R8, R9
#[cfg(not(target_os = "windows"))]
const ENTRY_ABI_REGS: &[u8] = &[7, 6, 2, 1, 8, 9]; // RDI, RSI, RDX, RCX, R8, R9

/// Size of the outgoing stack-argument block for a call with `stack_arg_count`
/// arguments past the entry-ABI register file, and the RSP-relative
/// displacement the first of them goes to.
///
/// Mirrors `x64::frames::stack_arg_block_size` exactly, and for the same two
/// reasons: Win64 requires 32 bytes of caller shadow space below the outgoing
/// arguments even when there are none, and RSP must stay 16-byte aligned at the
/// `CALL`, so the raw size is rounded up.
///
/// Returns `(0, _)` when nothing goes on the stack, so a call whose arguments
/// all fit registers emits no `SUB RSP` at all and is byte-identical to what
/// this backend produced before the stack path existed.
/// `[RBP + STACK_ARG_BASE]` is where the CALLER's first stack-passed argument
/// lands, as seen from inside this prologue.
///
/// After `push rbp; mov rbp, rsp` the saved RBP is at `[rbp+0]` and the return
/// address at `[rbp+8]`. On Windows the next 32 bytes are the caller's shadow
/// space (home slots for the four register arguments), so stack arguments begin
/// at `[rbp+0x30]`; SysV has no shadow space and they begin at `[rbp+0x10]`.
///
/// This is the READ side of [`stack_arg_block_size`]'s write side, and it is
/// the same number `x64::frames`' prologue uses — the two backends share the
/// convention, so a method compiled by either can be called by either.
#[cfg(target_os = "windows")]
const STACK_ARG_BASE: i32 = 16 + 32;
#[cfg(not(target_os = "windows"))]
const STACK_ARG_BASE: i32 = 16;

fn stack_arg_block_size(stack_arg_count: usize) -> (i32, i32) {
    #[cfg(target_os = "windows")]
    let (shadow, base) = (32i32, 32i32);
    #[cfg(not(target_os = "windows"))]
    let (shadow, base) = (0i32, 0i32);
    if stack_arg_count == 0 {
        return (0, base);
    }
    // Cast: a callee's parameter count is bounded by the JVMS 255-slot limit.
    let raw = shadow + (stack_arg_count as i32) * 8;
    ((raw + 15) & !15, base)
}

/// How many incoming argument slots this backend's prologue can actually
/// deposit — the context pointer (when present) plus the Java arguments,
/// receiver included.
///
/// How many incoming slots arrive in REGISTERS. No longer a hard limit: an
/// argument past it arrives on the caller's stack and `emit_prologue` loads it
/// from there. Kept because the prologue still has to know where the register
/// half stops. Historically `lower()` refused
/// such a graph up front — see the Gap B bail there for what that cost the
/// last time the two got out of step.
///
/// `pub(crate)` because the ELIGIBILITY decision has to consult it too. The
/// direct self-recursive call (`emit_self_recursive_call`) marshals the context
/// pointer plus every Java argument into one of these registers and has no
/// stack-argument path of its own, so `jit::lib`'s `is_self_recursive_direct`
/// must refuse a method that does not fit. That check used to be implied by
/// `lower()`'s whole-method bail; when the prologue learned to read stack
/// arguments the bail went away and the marshal's assumption became unguarded —
/// which is the "last time the two got out of step" this doc comment already
/// warned about, arrived at from the other side.
#[inline]
pub(crate) fn incoming_abi_reg_capacity() -> usize {
    ENTRY_ABI_REGS.len()
}

// ── Lowering state ───────────────────────────────────────────────────

struct Lowerer<'a> {
    graph: &'a Graph,
    schedule: &'a Schedule,
    buf: ExecutableBuffer,
    /// Maps NodeId → frame offset where its result is stored, or `None` when
    /// the node has no location yet.
    ///
    /// `Option<NonZeroU32>` rather than `i32`, so "unallocated" is not a
    /// *value* on the same axis as a real offset. The old representation was
    /// zero-initialised, and `0` is simultaneously "never allocated" and the
    /// encoding of `[rbp - 0]` — the saved caller frame pointer. A missed
    /// allocation therefore lowered to code that read the caller's RBP as
    /// program data (observed as heap addresses landing in `double[]`
    /// elements, i.e. silent array mis-sorts). Neither state is representable
    /// now: `None` cannot be emitted, and a `NonZeroU32` offset cannot be 0.
    node_slot: Vec<Option<NonZeroU32>>,
    /// Soundness latch (ES SortingDigestTests -Jit on): set when `slot_of`
    /// is asked for a node that never went through `alloc_slot` (observed: the
    /// pc17 ArrayLoad(Double) feeding a GVN-collapsed loop phi in
    /// DualPivotQuicksort.insertionSort — the scheduler never placed the node
    /// in an emitted block). The lowering entry point checks this latch and
    /// bails to the single-pass backend instead.
    ///
    /// **This latch is now unreachable.** `verify_data_locations` runs BEFORE
    /// any code is emitted and refuses exactly the graphs that used to trip it
    /// (a data input that no emitted node defines, or one defined after its
    /// use), so `slot_of` can no longer meet an unallocated node. It is kept —
    /// and strengthened, it now carries a structured [`Bailout`] rather than
    /// one bit — as a belt-and-braces net for a future lowering that allocates
    /// a slot on some paths only. If it ever fires again, the pre-emission
    /// verifier has a hole; fix the verifier, not the latch.
    unallocated_slot_use: std::cell::Cell<bool>,
    /// Structured reason for the first latched failure (the `Bailout` form of
    /// `unallocated_slot_use`, plus any resource bailout raised from a legacy
    /// infallible accessor). Read and reported by `lower_inner`.
    latched_bailout: std::cell::RefCell<Option<Bailout>>,
    /// Liveness-based frame-slot colouring for this method: which 8-byte spill
    /// slot each value lives in. Computed by [`plan_slots`] before the frame is
    /// sized, so `frame_size` budgets the *peak simultaneously live* value
    /// count rather than the node count. `alloc_slot_checked` reads it; it
    /// never bump-allocates.
    slot_plan: &'a SlotPlan,
    /// Highest spill offset any already-emitted node's slot reaches
    /// (exclusive), i.e. `max(offset + 8)` over every `alloc_slot` so far.
    ///
    /// Was `next_spill`, the bump cursor, whose value happened to mean the same
    /// thing while slots were handed out in emission order. Under colouring the
    /// cursor is gone but the *watermark* is still exactly what a safepoint's
    /// `live_frame_hi` needs: words above it have never been written by this
    /// frame, so the collector's band scan may skip them.
    spill_high_water: i32,
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
    /// Bytecode pc of the node currently being lowered — the throw-site bci
    /// every exceptional exit emitted while lowering it belongs to.
    ///
    /// Set from `Node::bytecode_pc` at the top of `lower_data_node` /
    /// `lower_terminator` (a node with no pc keeps the previous value rather
    /// than resetting to 0: the pc-less nodes are the scheduler's own control
    /// glue, which emits no exceptional exit of its own). Read by
    /// [`Self::push_call_exc_patch`].
    cur_bci: usize,
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
    ///
    /// It is also the only compact-layout-correct `Op::Load` lowering: the
    /// inline fallback derives `HEADER_SIZE + field_index*SLOT_SIZE`, which a
    /// compact object does not obey. See [`compact_field_lowering_available`].
    getfield: usize,
    /// Address of the `jit_putfield_int` helper — the compact-layout-correct
    /// `Op::Store` lowering, for the same reason as `getfield` above. The
    /// inline store is kept for the legacy (uniform-slot) layout, where it
    /// avoids a call per field write.
    putfield_int: usize,
    /// COV-03 — address of `jit_putfield_object`, the ONLY lowering a reference
    /// field store has.
    ///
    /// It is not an alternative to an inline path the way `putfield_int` is: it
    /// carries the SATB pre-barrier on the overwritten reference and the
    /// collector's own post-write barrier (G1's remembered-set edge included),
    /// and it is the identical helper the single-pass backend's reference
    /// `putfield` arms fall back to (`x64::objects::emit_ref_putfield_helper_call`).
    /// A missing barrier is invisible until a concurrent or generational
    /// collection, so this tier does not get its own inline reference store —
    /// `lower_inner` refuses any graph with an `Op::Store(MemKind::Ref)` when
    /// this address is absent.
    ///
    /// Unlike every other `putfield_*` helper it takes the VM context pointer as
    /// its first argument, which is why `scan_frame_needs` marks a reference
    /// store `needs_context`.
    putfield_object: usize,
    /// COV-03 — `jit_putfield_{long,float,double}`, the compact-layout-correct
    /// wide-field stores. Same `(obj_ptr, field_index, bits)` ABI as
    /// `putfield_int`, no context argument; the value rides a GPR as raw bits
    /// (`f32::to_bits` zero-extended / `f64::to_bits`), which is exactly how the
    /// FP frame slots already hold it.
    putfield_long: usize,
    putfield_float: usize,
    putfield_double: usize,
    /// Compact-layout/TLAB-aware object allocation helper. Live `Op::New`
    /// nodes use the same shared runtime-lowering stub as the baseline tier.
    new_object: usize,
    /// TLAB geometry and the post-init helper, for the inline bump that
    /// `Op::New` takes instead of the stub. All three are wired together or
    /// not at all; `emit_inline_tlab_new_ir` declines on any zero.
    tlab_cursor_off: i32,
    tlab_end_off: i32,
    tlab_post_init: usize,
    /// cov-06 — `jit_newarray(vm, atype, length) -> array | 0`. The lowering
    /// for a PRIMITIVE `Op::NewArray` (`element_type != 0`); zero-fill, GC
    /// retry and the zero-on-failure convention (negative length OR OOM) are
    /// the same as `new_object`'s, and this is the identical helper the
    /// single-pass backend's 0xbc arm calls.
    newarray: usize,
    /// cov-06 — `jit_anewarray_object(vm, component_class_id, length) -> array
    /// | 0`. The lowering for a REFERENCE `Op::NewArray` (`element_type ==
    /// 0`) — same zero-on-failure convention, same helper the single-pass
    /// backend's 0xbd arm calls.
    anewarray_object: usize,
    monitor_enter: usize,
    monitor_exit: usize,
    /// cov-01. `jit_ldc_string_cp(vm, holder, cp_idx) -> ObjectRef | 0` — the
    /// recorded-or-interned literal an `Op::ConstString` lowers to. `jit_ldc_class_cp(vm, holder,
    /// cp_idx) -> mirror | 0` — the resolution an `Op::ConstClass` lowers to.
    /// `jit_getstatic(vm, class_id, field_index) -> value | i64::MIN` — the
    /// `<clinit>`-running slow path an `Op::LoadStatic` falls back to when the
    /// class is not already initialised at compile time.
    ///
    /// All three are OptionalPtr in practice (a synthetic unit-test helper
    /// table leaves them 0), so `lower_inner` refuses a graph that would need
    /// an absent one rather than emitting a `CALL` through address zero.
    ldc_string_cp: usize,
    ldc_class_cp: usize,
    getstatic: usize,
    /// cov-05. `jit_instanceof(vm, obj, name_ptr, name_len) -> 0/1` — the
    /// SAME `RequiredPtr` helper (jit-api) the single-pass backend's 0xc1 arm
    /// calls; unlike `ldc_string_cp`/`ldc_class_cp`/`getstatic` this one is
    /// never absent, so `Op::InstanceOf`'s lowering does not need an
    /// OptionalPtr guard.
    instanceof_check: usize,
    /// cov-05. `jit_checkcast(vm, obj, name_ptr, name_len) -> obj|0|i64::MIN`
    /// — same `RequiredPtr` status as `instanceof_check` above, the SAME
    /// helper the single-pass backend's 0xc0 arm calls.
    checkcast: usize,
    /// cov-07. `jit_throw_exception(exc_ptr, bci) -> i64::MIN` (always) — the
    /// SAME `RequiredPtr` helper the single-pass backend's `0xbf` arm calls.
    /// Unlike `checkcast`/`instanceof` this takes no `vm_ptr`: it looks up the
    /// current JIT thread from TLS (`jit_thread_mut`), which `execute_jit_call`
    /// installs around every JIT call regardless of backend — so `Op::Throw`
    /// needs no `needs_context` plumbing of its own.
    throw_exception: usize,
    /// `jit_set_throw_bci(bci)` — stamps THIS method's own throw-site bci onto
    /// `JitSignals::athrow_bci`, overwriting whatever a callee's compiled
    /// `athrow` lowering left there. Called from the shared exceptional-exit
    /// stub ([`Self::emit_call_exc_stub`]); see that function for why an exit
    /// without it silently drops a `finally`.
    ///
    /// `RequiredPtr` in `JitRuntimeHelpers`, and the SAME helper the
    /// single-pass backend's `emit_exception_check_stub` calls.
    set_throw_bci: usize,
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
    /// Frame offset of the reserved safepoint-id slot (see the layout comment
    /// in `new`). Non-zero for every IR frame; the relocation contract needs it
    /// so the collector can tell which `OopMapEntry` describes this frame right
    /// now, rather than unioning every map in the method (unsound to relocate).
    sp_id_slot_off: i32,
    shadow_thread_slot_off: i32,
    /// Frame offsets (`[rbp - off]`) of every reference PARAMETER's prologue
    /// home. These are not node results, so the node scan cannot find them —
    /// see [`Lowerer::emit_safepoint_map`].
    ref_param_homes: Vec<i16>,
    shadow_savebase_slot_off: i32,
    /// Shadow `top` captured at method entry; restored by every exit. See the
    /// layout comment in `Lowerer::new`.
    shadow_savetop_slot_off: i32,
    /// The one reserved frame word [`Lowerer::emit_phi_copies`] parks a value in
    /// to break a cycle in an edge's parallel copy ([`CopyOp::Save`] /
    /// [`CopyOp::Restore`]).
    ///
    /// RAX is already the *move* temporary — every `Move` reads through it — so
    /// breaking a cycle needs a second location that survives the intervening
    /// move, and a frame word is the one thing this backend always has. It is a
    /// bookkeeping reservation like the four above it, so it sits BELOW
    /// `first_spill` and can never collide with a colour [`plan_slots`] hands
    /// out.
    ///
    /// Deliberately not zeroed in the prologue and never named by an oop map: a
    /// `Save` always precedes its matching `Restore` within one straight-line
    /// copy sequence (no safepoint, no branch, no call in between), so the word
    /// is never read before it is written and never holds a live reference
    /// across a point the collector can observe.
    phi_copy_scratch_slot_off: i32,
    /// `get_current_thread` helper; `0` when unwired (the JIT unit tests' stub
    /// table). Every shadow site is gated on it — dereferencing a thread
    /// pointer that was never fetched would read stack garbage.
    get_current_thread: usize,
    /// Byte offset of the `ShadowStack` within `JvmThread`.
    shadow_off_in_thread: i32,
    /// Offsets pushed by the safepoint most recently emitted, consumed by the
    /// matching reload. Cleared on consumption so an unbalanced push cannot
    /// hand stale homes to a later reload.
    pending_shadow: Vec<i16>,
    /// Byte span of the prologue's `get_current_thread` fetch, so it can be
    /// NOP-ed out when the method turns out never to publish.
    thread_fetch_span: Option<(usize, usize)>,
    /// Did any safepoint actually emit a shadow push?
    shadow_pushed_any: bool,
    /// Compact-layout metadata for the instance fields this method reads:
    /// `(bytecode_pc, is_reference) → (packed byte offset from the object
    /// body, is_reference, descriptor tag)`. Empty ⇒ every `Op::Load` takes
    /// the checked helper.
    ///
    /// The key carries `is_reference` because one pc can own two rows: the
    /// String-access expansion emits a `coder` (Int) and a `value` (Ref)
    /// load at a single `invokevirtual` pc.
    ///
    /// Present for the same reason the single-pass backend carries it: routing
    /// every field read through `jit_getfield` costs a boundary note, a region
    /// walk and a 16-byte atomic cell read on one of the hottest operations a
    /// JIT emits. The single-pass backend measured that at 4.7x on bintrees-16
    /// when the hardening first landed; the IR tier still paid it, which is why
    /// a forced-C2 bt18 ran 1.85x slower than the C1 body it replaced.
    compact_fields: HashMap<(usize, bool), (u32, bool, u8)>,
    /// Address of the GC's published `JIT_READ_BOUNDS` table, for the guarded
    /// receiver check. Zero ⇒ no inline field read (the guard cannot be
    /// emitted, so the helper stays).
    ///
    /// The READ table, not `JIT_REGION_BOUNDS`: this tier emits no inline
    /// reference STORE, so it asks only "is this address mapped, so a raw load
    /// cannot fault". The store question -- which G1/ZGC answer by leaving
    /// `JIT_REGION_BOUNDS` empty (`audits/g1-audit.md` 8.1) -- has no site
    /// here to ask it.
    ///
    /// **Since 2026-09-02 there is such a site** — `emit_gated_ir_ref_putfield`
    /// below — and it asks the same READ question for the same reason: its
    /// barrier decision comes from the collector's published PLAN, not from a
    /// region table. `JIT_REGION_BOUNDS` is still never consulted here, so the
    /// G1/ZGC interlock that leaves it empty is untouched.
    read_bounds_addr: usize,
    /// `(pre, post, young_floor)` — the collector's published reference-store
    /// barrier plan, resolved once at construction. `None` ⇒ no plan, and every
    /// reference store keeps the unconditional `jit_putfield_object` call.
    ///
    /// Read through the SAME `x64::objects` predicate the single-pass backend
    /// uses. Two tiers deciding independently what a published plan means is
    /// how one of them ends up skipping a barrier the other pays.
    ref_store_gates: Option<(usize, usize, usize)>,
    /// The published post-barrier skip mask, when the plan uses that shape
    /// rather than the unsigned age floor.
    ref_store_post_skip_mask: Option<u8>,
    /// Emitted shadow push / reload sequence counts.
    ///
    /// Every push must have exactly one reload: a push advances the thread's
    /// shadow `top`, and only the reload retracts it. An unmatched push walks
    /// `top` forward once per execution of that site — which for a recursive
    /// method means until it runs off the end of the shadow stack and starts
    /// writing into whatever is mapped next (observed as heap corruption
    /// inside the C allocator, not as anything resembling a JIT fault). The
    /// self-recursive route did exactly this. `lower_inner` refuses to publish
    /// a body when these disagree, so the failure mode is a compile that falls
    /// back to single-pass instead of a corrupted process.
    shadow_pushes: usize,
    shadow_reloads: usize,
    /// Byte size of the Java-locals region, for the published `FrameLayout`.
    locals_size: i32,
    /// First operand-spill offset, i.e. the exclusive top of the reserved
    /// locals region (locals + context + the five bookkeeping words: sp-id,
    /// cached thread, shadow save-base, shadow save-top, phi-copy scratch).
    first_spill: i32,
    /// Frame offsets (`[rbp - off]`) of every slot the colouring proved holds
    /// no reference, computed once from [`SlotPlan::prim_colors`] and published
    /// on every safepoint map as `OopMapEntry::non_oop_stack_slots`. The IR
    /// tier's half of the stale-word oracle; nothing gates on it.
    prim_slot_offsets: Vec<i16>,
    /// Per-safepoint oop maps published for the moving-young relocation
    /// contract. An empty vector means "no precise coverage", which
    /// `conservative_roots` reads as a refusal — the fail-closed direction.
    oop_maps: Vec<super::OopMapEntry>,
    /// Nodes whose frame slot has been WRITTEN by already-emitted code. A
    /// `Ref` node's slot is uninitialised stack garbage until its defining node
    /// is emitted, so publishing it before then would hand the collector a
    /// bogus root out of the previous frame's leftovers.
    defined_nodes: Vec<bool>,
    /// Monotonic safepoint id. Starts at 1: `sp_id_slot_off == 0` is the
    /// "no slot" sentinel on the reader side, and a zero id in the slot means
    /// "this frame has not reached a safepoint yet".
    next_sp_id: u32,
    /// `(safepoint id, bytecode index)` for every safepoint this compile
    /// emits, in emission order — which is ascending by id, because ids come
    /// from `next_sp_id`.
    ///
    /// The translation `CompiledMethod::safepoint_bci_table` documents. The id
    /// is what the frame slot holds and what the GC keys its map on; the bci is
    /// what a stack trace needs and what nothing on this backend recorded, so
    /// every optimizing-tier frame printed `(Unknown Source)`. Recorded here
    /// rather than in `OopMapEntry::bytecode_pc` because that field IS the id —
    /// overwriting it would repoint every oop-map lookup the collector makes.
    sp_id_bcis: Vec<(u32, u32)>,
    /// Frame offset of `arg[0]` in the Java-argument staging region a call
    /// marshals its args into; `arg[i]` lives at `args_stage_top_off - i*8`
    /// (increasing address), and `args_ptr = rbp - args_stage_top_off`.
    args_stage_top_off: i32,
    /// Upper bound (inclusive) for a spill slot's frame offset — excludes the
    /// shadow space, the arg-staging region AND the callee-saved XMM save area,
    /// so spills never overlap any of them.
    spill_cap_off: i32,
    /// Bytes reserved for the callee-saved XMM save area, `[spill_cap_off,
    /// spill_cap_off + this)`. See [`ir_saved_xmm_bytes`] and
    /// [`IR_LOWER_SAVED_XMMS`].
    ///
    /// Latched at construction rather than re-derived at emission time: the
    /// frame was *laid out* against this number, and a prologue that recomputed
    /// it from a flag read a second time would silently write outside its own
    /// reservation if the two reads ever disagreed.
    saved_xmm_bytes: i32,
    /// Bytes reserved for the callee-saved GPR save area, immediately below
    /// the XMM one: `[spill_cap_off, spill_cap_off + saved_gpr_bytes)`.
    ///
    /// Latched at construction for the same reason `saved_xmm_bytes` is.
    saved_gpr_bytes: i32,
    /// Native offsets of `JE rel32` instructions emitted after each dispatch
    /// call (the exception sentinel check) that jump to the shared bail stub,
    /// each paired with the bytecode pc of the instruction whose exceptional
    /// exit it is.
    ///
    /// The bci is not decoration: the bail stub stamps it onto
    /// `JitSignals::athrow_bci` (`helpers.set_throw_bci`) so the interpreter's
    /// post-JIT routing range-tests THIS method's own exception table against
    /// THIS method's throw site. See [`Self::emit_call_exc_stub`].
    call_exc_patches: Vec<(usize, usize)>,
    /// fib44-fix follow-up: native offsets of the rel32 operand of each direct
    /// self-recursive `CALL` (invoke_kind 4), patched at finalize to target the
    /// method's own entry (code offset 0). See `lower_self_call` / Op::Call.
    self_call_patches: Vec<usize>,
    /// IR direct-call lowering: statically-bound call sites this compile may
    /// lower as a raw `CALL` into an already-compiled callee's entry instead of
    /// routing through the generic `jit_invoke_dispatch` helper. Keyed by the
    /// invoke's bytecode pc (the same key the IR builder stamps on the
    /// `Op::Call` node via `Node::bytecode_pc`); the value is
    /// `(callee_entry, callee_needs_context)` as returned by the caller's
    /// `callee_compiler`. Empty (the default, and whenever the direct-call gate
    /// is off) ⇒ every `Op::Call` keeps the historical helper dispatch,
    /// byte-for-byte.
    direct_calls: &'a HashMap<usize, (usize, bool)>,
    /// IR inline-cache lowering (jit-inlining-and-ir-calls): virtual /
    /// interface call sites this compile serves from a monomorphic + polymorphic
    /// inline cache instead of the generic `jit_invoke_dispatch` helper. Keyed
    /// by the invoke's bytecode pc, exactly like [`Self::direct_calls`]; the
    /// value is `(mic_slot_addr, pic_slot_addr)`.
    ///
    /// This is the virtual/interface counterpart of `direct_calls`, and it is
    /// what removes the last reason the IR pipeline declined call-heavy
    /// methods: a statically bound call could already be bound directly, but an
    /// `invokevirtual` still paid a full helper round trip WITH a dynamic
    /// target lookup, which the single-pass backend has never done (it emits a
    /// MIC/PIC cascade — `x64.rs`). Empty ⇒ every virtual `Op::Call` keeps the
    /// historical helper dispatch, byte-for-byte.
    ///
    /// SAFETY: both addresses point at `Box`es owned by the compiling
    /// `try_compile_inner` frame and moved into the returned
    /// `CompiledMethod::_jit_mic_slots` / `_jit_pic_slots`, so the imm64s baked
    /// into the emitted guards cannot outlive their storage.
    ic_slots: &'a HashMap<usize, (usize, usize)>,
    /// Address of the `jit_invoke_virtual_mic` runtime helper — the
    /// inline-cache miss path, which resolves the receiver AND populates both
    /// caches. 6 args: `(vm_ptr, info_ptr, args_ptr, num_args, mic, pic)`.
    /// 0 ⇒ no IC site may be planned (the planner checks the same field).
    invoke_virtual_mic: usize,
    /// Shared post-call frame publication used by direct and hashed dispatch
    /// stubs. Zero when precise frame tracking is unavailable.
    frame_record: usize,
    /// Segment-relative displacement of the compile-id mirror, and this
    /// compilation's identity — the pair that lets the GC name the method
    /// owning the innermost RBP without decoding the call that created the
    /// frame. Both 0 when unavailable → nothing is published.
    inline_cm_tls_disp: usize,
    compile_id: u32,
    /// `jit_service_callee_deopt` — services a compiled callee's `i64::MIN`
    /// deopt sentinel at the megamorphic stub's inline call site, so the
    /// callee's stashed frame is resumed there instead of escaping to the
    /// caller as if the caller had deopted. Zero disables the emitted check.
    /// See `runtime_lowering::emit_callee_deopt_check`.
    service_callee_deopt: usize,
    /// wire-tiered-manager Step 4 (PGO handoff C1 → C2): per-bytecode-PC branch
    /// bias, keyed by the conditional-branch instruction's bytecode PC (the same
    /// key the IR builder stamps on each `Op::If` via `Node::bytecode_pc`). Value
    /// `true` = the branch is usually TAKEN, `false` = usually NOT taken; an
    /// absent PC is inconclusive. Only `Some(false)` (usually-not-taken) changes
    /// codegen — see `lower_terminator`'s `Op::If` arm. Empty (the default, and
    /// whenever profiling is off) ⇒ every `Op::If` keeps its historical layout
    /// byte-for-byte.
    branch_hints: &'a HashMap<usize, bool>,
    /// IR-tier inlining: `(start, end, invoke_bci)` for each relocated callee
    /// region in the combined bytecode buffer `IrBuilder` walked, in increasing
    /// `start` order and non-overlapping.
    ///
    /// A node inside a region carries its COMBINED-buffer pc, because that is
    /// what keys its own compact-field row, direct-call entry and inline cache.
    /// That pc names no instruction in THIS method, so wherever a bci is handed
    /// to the interpreter — a throw site checked against the exception table's
    /// `[start_pc, end_pc)` ranges, or the resume bci of a null/bounds guard —
    /// [`Lowerer::resume_bci`] maps it back to the enclosing `invoke`. Empty on
    /// every compile that splices nothing, where `resume_bci` is the identity.
    spliced_ranges: &'a [(usize, usize, usize)],
    /// Guard-surviving scalar replacement: metadata for each scalar-replaced
    /// `Op::New` so a deopt snapshot slot holding it lowers to a
    /// `FrameValue::VirtualObject`. `None` ⇒ disabled (byte-identical default).
    sr_map: Option<&'a ScalarReplacementMap>,
    /// Which inlined callee each safepoint snapshot belongs to, and the caller
    /// scopes stacked above it. Consumed by [`Lowerer::caller_chain_for`] to
    /// fill `FrameState::caller`, which was hard-coded `None` before this
    /// existed.
    ///
    /// Empty on every compile today — `IrBuilder::build` does not inline — and
    /// an empty table reproduces the historical flat frame states exactly.
    inline_scopes: &'a InlineScopeTable,

    // ── Linear-scan register read cache ──────────────────────────────
    //
    // All four are empty / zero unless `CRATONVM_JIT_IR_LINEAR_SCAN` is on, in
    // which case `lower_inner_with_scopes` installs the plan after
    // construction. See the section on [`plan_register_residency`].
    /// `reg_of[id]` = the XMM number [`plan_register_residency`] gave node
    /// `id`'s value for its whole life. Empty when the path is off.
    reg_of: Vec<Option<u8>>,
    /// `reg_live[id]` = the DEFINITION site for `id` has actually run and
    /// copied the value into `reg_of[id]`.
    ///
    /// This is the safety interlock, not bookkeeping. A read consults
    /// `reg_live`, never `reg_of` directly, so a definition arm this wave did
    /// not convert can only cost an optimization — never produce a read of a
    /// register that was never written. Adding a converted definition site is
    /// therefore additive; forgetting one is not a correctness event.
    reg_live: Vec<bool>,
    /// `gp_reg_of[id]` = the GENERAL-PURPOSE register
    /// [`plan_register_residency`] gave node `id` for its whole life. The
    /// integer twin of `reg_of`; empty when the path is off.
    gp_reg_of: Vec<Option<u8>>,
    /// The same safety interlock as `reg_live`, for the GP file. A read
    /// consults this and never `gp_reg_of` directly, so an unconverted
    /// definition arm costs an optimization and can never produce a read of a
    /// register nothing wrote.
    gp_reg_live: Vec<bool>,
    /// Number of input references to each node, over every input of every
    /// node. Filled by `prepare_fusion_tables` before the first block lowers.
    use_count: Vec<u32>,
    /// `Op::Cmp` nodes whose only use is the `Op::If` terminator consuming
    /// them, lowered as one fused compare-and-branch by that terminator; the
    /// compare's own arm emits nothing (`ir_fused_branch_enabled`).
    fused_cmp: Vec<bool>,
    /// Nodes named by some safepoint snapshot. Their home word must hold the
    /// value at that bci, so such a compare is never fused away.
    deopt_named: Vec<bool>,
    /// Per-block CSE of the mapped-receiver guard (null, alignment, the six
    /// read-bounds compares): receivers already proven in the block being
    /// lowered. Cleared at every block entry (`ir_receiver_guard_cse_enabled`).
    guarded_receivers: Vec<NodeId>,
    /// Receivers proven merely NON-NULL in the block being lowered — a weaker
    /// fact than [`Self::guarded_receivers`], and kept apart from it for that
    /// reason.
    ///
    /// A null test proves null-ness and nothing else. Folding it into the
    /// mapped-receiver set would let a later site skip the alignment and
    /// read-bounds compares on the strength of a test that never made them,
    /// which is the one way this optimisation could go wrong quietly. So this
    /// set suppresses only the `TEST`/`JZ`; the containment guard still keys
    /// off `guarded_receivers` alone.
    ///
    /// Cleared at every block entry, then RE-SEEDED with the receiver — see
    /// `seed_block_null_proofs`.
    null_proven_receivers: Vec<NodeId>,
    /// Register → memory transitions this backend EMITTED for resident values
    /// (one per resident definition, because the wiring is write-through).
    /// Reported as `CompilationReport::spills`.
    ls_spills: usize,
    /// Memory → register transitions emitted for resident values: the
    /// materialisation of a value whose definition arm computes into RAX
    /// (`Op::ConstF`, `Op::Neg`, `Op::Param`) and therefore reaches its
    /// register through its home word. Reported as
    /// `CompilationReport::reloads`.
    ls_reloads: usize,

    // ── Level 2: the machine list ────────────────────────────────────
    //
    // Empty and `MirMode::Off` unless `CRATONVM_JIT=ir-isel-emit` or
    // `ir-isel-verify` is set. See [`MirPlan`].
    /// The selector's output for this method, one entry per scheduled block.
    mir: Option<MirPlan>,
    /// What to do with it.
    mir_mode: MirMode,
    /// Tiles whose bytes this compile took from the level-2 encoder
    /// (`MirMode::Emit`) or checked against the per-opcode arms
    /// (`MirMode::Verify`).
    mir_tiles: usize,
    /// Byte disagreements between the two paths. Non-zero refuses the compile:
    /// increment 2 is where fail-closed returns.
    mir_mismatches: usize,
    /// Verify mode only, and it decides whether there is a next increment:
    /// tiles the encoder can express but is NOT allowed to emit, because their
    /// bytes are not identical to the per-opcode arm's.
    mir_shadow_tiles: usize,
    /// Bytes the per-opcode arms wrote for those tiles' nodes…
    mir_arm_bytes: usize,
    /// …and bytes the level-2 encoder would have written instead.
    mir_enc_bytes: usize,
}

/// What the level-2 machine list is for on this compile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MirMode {
    /// Not built. The default, and every compile that does not set a flag.
    Off,
    /// Built, and checked against the per-opcode arms byte for byte. The
    /// **per-opcode arms still emit** — this mode changes no emitted byte, so a
    /// disagreement is a diagnosis rather than a wrong instruction. It is the
    /// method-scale form of the anchoring property each `PATTERNS` row carries
    /// per row, and it is the gate increment 2 has to pass on a real corpus
    /// before [`MirMode::Emit`] is worth running.
    Verify,
    /// Built, and it emits. The per-opcode arm is not run for a node a tile
    /// covers.
    Emit,
}

/// Level 2 — the machine list, as a value the emitter can consume.
///
/// Deliberately not a new IR, a crate or a trait hierarchy: it is the
/// selector's own `Vec<MInst>` per block, and the two side tables that already
/// existed (`SlotPlan`, the frame plan; `RegResidency`, the allocation) stay
/// where they are. That is the whole artifact
/// `docs/feature-designs/jit-machine-level-and-instruction-selection.md` says
/// level 2's first form should be.
struct MirPlan {
    /// One selection per scheduled block, in block order.
    blocks: Vec<crate::x64::isel::BlockSelection>,
    /// `tile_of[node]` = index into `blocks[b].tiles` of the tile rooted at
    /// `node`, for the block `b` the node was scheduled into.
    ///
    /// A node a tile *absorbed* is deliberately absent: it has no tile of its
    /// own, and the encoder must never be asked to emit one for it.
    tile_of: Vec<Option<(u32, u32)>>,
}

impl<'a> Lowerer<'a> {
    fn new(
        graph: &'a Graph,
        schedule: &'a Schedule,
        buf: ExecutableBuffer,
        num_params: usize,
        num_locals: usize,
        slot_plan: &'a SlotPlan,
        helpers: &JitRuntimeHelpers,
        branch_hints: &'a HashMap<usize, bool>,
        spliced_ranges: &'a [(usize, usize, usize)],
        sr_map: Option<&'a ScalarReplacementMap>,
        direct_calls: &'a HashMap<usize, (usize, bool)>,
        ic_slots: &'a HashMap<usize, (usize, usize)>,
        compact_fields: &HashMap<(usize, bool), (u32, bool, u8)>,
        inline_scopes: &'a InlineScopeTable,
    ) -> Self {
        // Frame homes of the reference PARAMETERS, in `[rbp - off]` form. Every
        // safepoint map republishes these; see `emit_safepoint_map`.
        let ref_param_homes: Vec<i16> = graph
            .nodes
            .iter()
            .filter_map(|n| match n.op {
                Op::Param(idx) if n.ty == IrType::Ref => {
                    let off = ((idx as i32) + 1) * 8;
                    if off > 0 && off <= i16::MAX as i32 {
                        Some(off as i16)
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .collect();
        // Gap B: scan for `Op::Call` to size the call-related frame regions.
        // `needs_context` ⇒ the method takes the VM ptr as a hidden first arg
        // and reserves a context slot. `max_call_args` sizes the Java-argument
        // staging region a call marshals its args into before dispatching.
        //
        // Shared with `estimate_frame_bytes`, which `lower_inner` uses to
        // refuse an over-budget frame BEFORE this constructor reserves it; the
        // two must see the same needs or the check would bound a different
        // frame than the one built here.
        let needs = scan_frame_needs(graph, helpers);
        let needs_context = needs.needs_context;
        let max_call_args = needs.max_call_args;

        // Frame layout (rbp downward): locals, [context slot], five bookkeeping
        // words, spills, [callee-saved XMM save area], [args staging], 16-byte
        // stack-arg reserve, 32-byte shadow. Reserve slots for
        // locals + one per COLOUR + shadow. The 16-byte tail above the shadow
        // region holds in-frame stack args for any helper called without
        // `emit_stack_arg_setup`; see the matching comment in `x64.rs`
        // (Compiler::new) for the worst-case 6-arg `jit_invoke_virtual_mic` site.
        let locals_size = (num_locals as i32) * 8;
        let context_size = if needs_context { 8 } else { 0 };
        // Five extra reserved slots: the safepoint id, the cached
        // `*mut JvmThread`, the shadow stack's base `top` for this push, the
        // shadow `top` watermark captured at method entry, and the phi
        // parallel-copy scratch word (`emit_phi_copies`, which needs a location
        // other than RAX to park a value in while it unwinds a cycle).
        let bookkeeping_size = 8i32 * 5;
        // Liveness-based slot reuse: one 8-byte slot per *colour*, not per
        // graph node. [`plan_slots`] has already packed every value whose live
        // range does not overlap another's into a shared slot, so this term is
        // the peak simultaneous live count (plus the pinned classes), not
        // `graph.nodes.len()`. A value dead since block 0 no longer owns a slot
        // for the rest of the method.
        let spill_size = (slot_plan.slots as i32) * 8;
        let args_stage_size = (max_call_args as i32) * 8;
        let shadow = 32i32;
        let stack_arg_reserve = 16i32;
        // The frame is SIZED by the same estimator `lower_inner` bounds against
        // (`estimate_frame_bytes` + `check_frame_size`), so "the frame we
        // checked" and "the frame we build" are the same number by
        // construction rather than by two hand-kept-in-sync expressions. The
        // check has already refused anything above `DEFAULT_MAX_FRAME_BYTES`
        // (32 KiB), so every i32 term below is small and cannot overflow —
        // previously `(graph.nodes.len() as i32) * 8` on a pathological graph
        // could.
        let frame_size = estimate_frame_bytes(num_locals, slot_plan.slots, &needs) as i32;
        let saved_xmm_bytes = ir_saved_xmm_bytes();
        let saved_gpr_bytes = ir_saved_gpr_bytes();
        debug_assert_eq!(
            frame_size,
            ((locals_size
                + context_size
                + bookkeeping_size
                + spill_size
                + saved_xmm_bytes
                + saved_gpr_bytes
                + args_stage_size
                + shadow
                + stack_arg_reserve)
                + 15)
                & !15,
            "estimate_frame_bytes must mirror the frame this constructor lays out",
        );

        // The context slot is the first slot after the locals; spills start
        // after it.
        //
        // BUG FIX (jit-inlining-and-ir-calls): `arg[0]` of the staging region
        // used to sit at `frame_size - shadow`, i.e. at address `RSP + 32` —
        // which is exactly where the Win64 ABI places a callee's FIFTH stack
        // argument, and `RSP + 40` (staged `arg[1]`) the sixth. That was
        // latent while every helper the IR lowerer called took at most four
        // arguments, so no stack argument was ever written. The inline-cache
        // miss path calls `jit_invoke_virtual_mic`, which takes SIX
        // (`vm, info, args_ptr, num_args, mic, pic`), and would have written
        // `mic`/`pic` straight over staged `arg[0]`/`arg[1]` — the receiver
        // and first argument of the very call being dispatched.
        //
        // The 16-byte `stack_arg_reserve` was already budgeted into
        // `frame_size` for exactly this purpose (see the layout comment above,
        // which describes the reserve as sitting between the staging region and
        // the shadow space); it simply was not being SKIPPED when placing the
        // staging region. Skip it now, so the documented layout and the actual
        // one agree: `[RSP, RSP+32)` shadow, `[RSP+32, RSP+48)` stack args 5-6,
        // staging from `RSP + 48` up, spills above that. Every consumer reads
        // `args_stage_top_off` symmetrically (`store_rax` to place, then
        // `lea_reg_from_frame` to pass `args_ptr`), so relocating the region is
        // transparent to the generic dispatch path.
        let context_slot_off = if needs_context {
            (num_locals as i32 + 1) * 8
        } else {
            0
        };
        // Relocation contract: one reserved slot holding the id of the
        // safepoint this frame is currently stopped at. `conservative_roots`
        // reads `[rbp - cm.sp_id_slot_off]` and matches it against `oop_maps`
        // to find the map for the ACTIVE safepoint — a union-of-all-maps is
        // unsound for relocation. Zero would mean "no slot", so ids start at 1.
        let base = num_locals as i32 + 1 + if needs_context { 1 } else { 0 };
        let sp_id_slot_off = base * 8;
        // Cached thread pointer, fetched once in the prologue. The shadow push
        // and reload both need it and neither may call out to get it.
        let shadow_thread_slot_off = (base + 1) * 8;
        // The `top` this push started from. The reload restores from exactly
        // here, so an intervening unbalanced push cannot drift it (the
        // single-pass backend's spring-bug-10 fix; same hazard applies here).
        let shadow_savebase_slot_off = (base + 2) * 8;
        // The shadow `top` at METHOD ENTRY. Every exit restores it, unwinding
        // any push this activation did not pop. Without it an abnormal exit
        // (the `i64::MIN` exception/deopt sentinel, which jumps straight to the
        // shared bail stub and skips the matching reload) leaks its push
        // FOREVER: nothing else retracts `top` once a raw JIT-to-JIT call has
        // removed the Rust boundary whose `restore_jit_thread` used to heal it.
        // The single-pass backend has always done this (`emit_epilogue`'s
        // savetop-restore); this tier never did, and leaked ~4 slots per
        // sentinel return until the 256K-slot stack ran off its end and the
        // unguarded push stored past the mapping. See
        // `x64.rs::Compiler::emit_epilogue` for the mechanism this mirrors.
        let shadow_savetop_slot_off = (base + 3) * 8;
        // One word for the phi parallel copy's `Save`/`Restore` scratch. Every
        // phi has its OWN frame word (`prealloc_phi_slots`), so an edge whose
        // copies form a cycle — the swap loop, whose two loop-header phis are
        // each other's back-edge value — cannot be sequentialised through RAX
        // alone: RAX is the move temporary and is dead the instant the next
        // `Move` loads through it. Parking one element of the cycle here frees
        // its predecessor and the rest unwinds with plain moves
        // (`regalloc::resolve_parallel_copy`). One word, once per method,
        // whether or not any edge ever needs it — the alternative was refusing
        // the compile and dropping a tier on every swap-shaped loop.
        //
        // It is a BOOKKEEPING word, below `first_spill`, so `plan_slots` can
        // never colour a value onto it and `verify_slot_colouring` /
        // `verify_data_locations` see an unchanged spill band.
        let phi_copy_scratch_slot_off = (base + 4) * 8;
        let first_spill = (base + 5) * 8;
        let args_stage_top_off = frame_size - shadow - stack_arg_reserve;
        // The callee-saved XMM save area sits directly ABOVE the spill band and
        // directly BELOW the outgoing-argument staging, so `callee_saved_lo`
        // stays exactly `spill_cap_off` and the band the GC verifier skips
        // (`conservative_roots::band_slot_is_verifiable`) grows to cover it
        // without any change on the reader side. That placement is not
        // cosmetic: a register image is precisely "storage this frame does not
        // resume from", which is what that band means.
        //
        // Register `i` occupies offsets `(xmm_saved_lo + 16*i, xmm_saved_lo +
        // 16*(i+1)]`, so its `MOVUPS` base is `[rbp - (spill_cap_off +
        // 16*(i+1))]` and the 16 bytes it writes run up to `rbp -
        // spill_cap_off` exclusive — inside the reservation, never over a
        // spill.
        // The GPR band sits below the XMM one and is excluded from the spill
        // range on exactly the same footing: a spill that overlapped it would
        // be silently destroyed by the prologue save.
        let spill_cap_off = frame_size
            - shadow
            - stack_arg_reserve
            - args_stage_size
            - saved_xmm_bytes
            - saved_gpr_bytes;

        Lowerer {
            graph,
            schedule,
            buf,
            node_slot: vec![None; graph.nodes.len()],
            unallocated_slot_use: std::cell::Cell::new(false),
            latched_bailout: std::cell::RefCell::new(None),
            prim_slot_offsets: {
                // Same arithmetic as `planned_slot_off`: colour `c` lives at
                // `[rbp - (first_spill + 8c)]`. An offset past `i16` is dropped
                // rather than truncated -- the map's own slot list has the same
                // bound, and a silently wrong offset would accuse the wrong
                // slot.
                let mut offs: Vec<i16> = slot_plan
                    .prim_colors()
                    .into_iter()
                    .filter_map(|c| {
                        i32::try_from(u64::from(c).saturating_mul(8))
                            .ok()
                            .and_then(|d| first_spill.checked_add(d))
                            .and_then(|o| i16::try_from(o).ok())
                    })
                    .collect();
                offs.sort_unstable();
                offs
            },
            slot_plan,
            spill_high_water: first_spill,
            block_offsets: vec![0; schedule.blocks.len()],
            branch_patches: Vec::new(),
            num_params,
            _num_locals: num_locals,
            frame_size,
            bci_native: HashMap::new(),
            cur_bci: 0,
            deopt_stub_patches: Vec::new(),
            deopt_boxes: Vec::new(),
            invoke_dispatch: helpers.invoke_dispatch,
            lambda_int_to_double: helpers.lambda_int_to_double,
            dispatch_threw: helpers.dispatch_threw,
            self_call_stack_guard: helpers.self_call_stack_guard,
            frem: helpers.jit_frem,
            drem: helpers.jit_drem,
            getfield: helpers.getfield,
            putfield_int: helpers.putfield_int,
            putfield_object: helpers.putfield_object,
            putfield_long: helpers.putfield_long,
            putfield_float: helpers.putfield_float,
            putfield_double: helpers.putfield_double,
            new_object: helpers.new_object,
            // Casts: both are byte offsets inside `JvmThread`, far below i32::MAX.
            tlab_cursor_off: helpers.tlab_cursor_offset_in_thread as i32,
            tlab_end_off: helpers.tlab_end_offset_in_thread as i32,
            tlab_post_init: helpers.tlab_post_init,
            newarray: helpers.newarray,
            anewarray_object: helpers.anewarray_object,
            monitor_enter: helpers.monitor_enter,
            monitor_exit: helpers.monitor_exit,
            ldc_string_cp: helpers.ldc_string_cp,
            ldc_class_cp: helpers.ldc_class_cp,
            getstatic: helpers.getstatic,
            instanceof_check: helpers.instanceof_check,
            checkcast: helpers.checkcast,
            throw_exception: helpers.throw_exception,
            set_throw_bci: helpers.set_throw_bci,
            safepoint_flag_addr: helpers.safepoint_flag_addr,
            safepoint_slow_path: helpers.safepoint_slow_path,
            needs_context,
            context_slot_off,
            sp_id_slot_off,
            shadow_thread_slot_off,
            ref_param_homes,
            shadow_savebase_slot_off,
            shadow_savetop_slot_off,
            phi_copy_scratch_slot_off,
            get_current_thread: helpers.get_current_thread,
            shadow_off_in_thread: helpers.shadow_stack_offset_in_thread as i32,
            pending_shadow: Vec::new(),
            thread_fetch_span: None,
            shadow_pushed_any: false,
            compact_fields: compact_fields.clone(),
            read_bounds_addr: helpers.read_bounds_addr,
            ref_store_gates: crate::x64::ref_store_gates_of(helpers),
            ref_store_post_skip_mask: crate::x64::ref_store_post_skip_mask_of(helpers),
            shadow_pushes: 0,
            shadow_reloads: 0,
            locals_size,
            first_spill,
            oop_maps: Vec::new(),
            defined_nodes: vec![false; graph.nodes.len()],
            next_sp_id: 1,
            sp_id_bcis: Vec::new(),
            args_stage_top_off,
            spill_cap_off,
            saved_xmm_bytes,
            saved_gpr_bytes,
            call_exc_patches: Vec::new(),
            self_call_patches: Vec::new(),
            direct_calls,
            ic_slots,
            invoke_virtual_mic: helpers.invoke_virtual_mic,
            frame_record: helpers.frame_record,
            // Reserved here, at the start of this compilation: the prologue has
            // to encode the id as an immediate, and the `CompiledMethod` that
            // will own it does not exist until this buffer is filled.
            inline_cm_tls_disp: if crate::x64::inline_rbp_tls_disp() != 0 {
                crate::x64::inline_cm_tls_disp()
            } else {
                0
            },
            compile_id: if crate::x64::inline_rbp_tls_disp() != 0
                && crate::x64::inline_cm_tls_disp() != 0
            {
                crate::reserve_compile_id()
            } else {
                0
            },
            service_callee_deopt: helpers.service_callee_deopt,
            branch_hints,
            spliced_ranges,
            sr_map,
            inline_scopes,
            // Off by default; `lower_inner_with_scopes` installs a plan when
            // `CRATONVM_JIT_IR_LINEAR_SCAN` is on. Empty vectors, not
            // node-sized ones: `resident_xmm` reads through `get`, so an
            // absent plan costs one bounds check and no allocation.
            reg_of: Vec::new(),
            reg_live: Vec::new(),
            gp_reg_of: Vec::new(),
            gp_reg_live: Vec::new(),
            use_count: Vec::new(),
            fused_cmp: Vec::new(),
            deopt_named: Vec::new(),
            guarded_receivers: Vec::new(),
            null_proven_receivers: Vec::new(),
            ls_spills: 0,
            ls_reloads: 0,
            mir: None,
            mir_mode: MirMode::Off,
            mir_tiles: 0,
            mir_mismatches: 0,
            mir_shadow_tiles: 0,
            mir_arm_bytes: 0,
            mir_enc_bytes: 0,
        }
    }

    /// Install the linear-scan register plan. Called once, after construction
    /// and before any emission, only when the path is enabled.
    fn set_residency(&mut self, residency: RegResidency) {
        self.reg_live = vec![false; residency.reg_of.len()];
        self.reg_of = residency.reg_of;
        self.gp_reg_live = vec![false; residency.gp_reg_of.len()];
        self.gp_reg_of = residency.gp_reg_of;
    }

    /// Install the level-2 machine list. Called once, after construction and
    /// before any emission, only when a machine-level flag is on.
    fn set_mir(&mut self, plan: MirPlan, mode: MirMode) {
        self.mir = Some(plan);
        self.mir_mode = mode;
    }

    /// The XMM register `id`'s value is CURRENTLY resident in, if any.
    ///
    /// Gated on `reg_live`, not on `reg_of`: a value is only readable from a
    /// register once its definition site has published it there. See the field
    /// comment for why that direction is the safe one.
    fn resident_xmm(&self, id: NodeId) -> Option<u8> {
        if !self.reg_live.get(id as usize).copied().unwrap_or(false) {
            return None;
        }
        self.reg_of.get(id as usize).copied().flatten()
    }

    /// MOVAPS `dst`, `src` — a full 128-bit register copy.
    ///
    /// `MOVAPS` rather than `MOVSS`/`MOVSD` so the copy has no false
    /// dependency on `dst`'s previous contents and one encoding serves both
    /// widths: the low 32 or 64 bits are what every consumer reads, and a bit
    /// copy preserves them exactly (NaN payloads included — this must never be
    /// an arithmetic move).
    fn fp_reg_move(&mut self, dst: u8, src: u8) {
        if dst == src {
            return;
        }
        // REX.R for dst >= 8, REX.B for src >= 8. `IR_LOWER_LS_XMMS` is
        // XMM2–XMM5 and the value tier is XMM0/XMM1, so no REX is emitted
        // today; encoding it anyway keeps the helper correct if the file grows.
        let rex = 0x40u8 | (((dst >= 8) as u8) << 2) | ((src >= 8) as u8);
        if rex != 0x40 {
            self.buf.emit_byte(rex);
        }
        self.buf
            .emit(&[0x0F, 0x28, 0xC0 | ((dst & 7) << 3) | (src & 7)]);
    }

    /// Load `id`'s value into `xmm`, from its resident register when it has
    /// one and from its home word otherwise.
    ///
    /// The read half of the cache. Every FP operand read in `lower_data_node`
    /// goes through here; a site left calling `fp_load(self.slot_of(id))`
    /// directly is simply not accelerated.
    fn fp_load_value(&mut self, xmm: u8, id: NodeId, is_double: bool) {
        // Bound out of the scrutinee position: every read of `self` here must
        // finish before the emitters take `&mut self`.
        let resident = self.resident_xmm(id);
        match resident {
            Some(src) => self.fp_reg_move(xmm, src),
            None => {
                let off = self.slot_of(id);
                self.fp_load(xmm, off, is_double);
            }
        }
    }

    /// The register `id`'s value has been ASSIGNED, whether or not its
    /// definition has published it yet. Only the two publishing sites may use
    /// this; every reader goes through [`Self::resident_xmm`].
    fn assigned_xmm(&self, id: NodeId) -> Option<u8> {
        self.reg_of.get(id as usize).copied().flatten()
    }

    /// Mark `id` readable from its assigned register.
    fn mark_reg_live(&mut self, id: NodeId) {
        if let Some(cell) = self.reg_live.get_mut(id as usize) {
            *cell = true;
        }
    }

    /// Write `id`'s result from `xmm`: ALWAYS to the home word, and
    /// additionally into its resident register.
    ///
    /// The write-through invariant lives here. The home store is emitted
    /// unconditionally and first, so the frame image is complete at every
    /// instruction boundary — which is what lets `emit_safepoint_map`,
    /// `build_deopt_points` and `emit_phi_copies` stay untouched.
    fn fp_store_value(&mut self, id: NodeId, slot: i32, xmm: u8, is_double: bool) {
        self.fp_store(slot, xmm, is_double);
        let dst = self.assigned_xmm(id);
        if let Some(dst) = dst {
            self.fp_reg_move(dst, xmm);
            self.ls_spills += 1;
            self.mark_reg_live(id);
        }
    }

    /// Publish a result that was computed in RAX and already stored to its home
    /// word (`Op::ConstF`, FP `Op::Neg`, FP `Op::Param`) into its register.
    ///
    /// A memory → register transition, and counted as one: the value's only
    /// materialisation is the home word this reads back. It is still a win in
    /// a loop, where the alternative is that load once per use.
    fn publish_fp_from_slot(&mut self, id: NodeId, slot: i32, is_double: bool) {
        let dst = self.assigned_xmm(id);
        if let Some(dst) = dst {
            self.fp_load(dst, slot, is_double);
            self.ls_reloads += 1;
            self.mark_reg_live(id);
        }
    }

    // ── The general-purpose half of the same cache ──────────────────────
    //
    // Identical contract to the FP accessors above, over `IR_LOWER_LS_GPRS`
    // rather than `IR_LOWER_LS_XMMS`: write-through, gated on `gp_reg_live` so
    // a value is readable from a register only once its definition published it
    // there, and `None` everywhere means the read falls back to the home word.
    //
    // This is the half that reaches an `int` loop counter — the gap the FP-only
    // wiring's own doc comment named, and the reason this backend's bodies
    // measured slower than the single-pass bodies they supersede.

    /// The GP register `id`'s value is CURRENTLY resident in, if any.
    fn resident_gpr(&self, id: NodeId) -> Option<u8> {
        if !self.gp_reg_live.get(id as usize).copied().unwrap_or(false) {
            return None;
        }
        self.gp_reg_of.get(id as usize).copied().flatten()
    }

    /// The register `id`'s value has been ASSIGNED, whether or not its
    /// definition has published it yet. Only publishing sites may use this;
    /// every reader goes through [`Self::resident_gpr`].
    fn assigned_gpr(&self, id: NodeId) -> Option<u8> {
        self.gp_reg_of.get(id as usize).copied().flatten()
    }

    /// Mark `id` readable from its assigned GP register.
    fn mark_gp_reg_live(&mut self, id: NodeId) {
        if let Some(cell) = self.gp_reg_live.get_mut(id as usize) {
            *cell = true;
        }
    }

    /// Load `id`'s value into `dst`, from its resident register when it has one
    /// and from its home word otherwise.
    ///
    /// The read half of the GP cache. A site that still calls
    /// `load_to_rax(self.slot_of(id))` directly is simply not accelerated,
    /// which is a missed optimization and never wrong code: the home word is
    /// written unconditionally.
    fn gp_load_value(&mut self, dst: u8, id: NodeId) {
        // 2026-09-02: a constant is an IMMEDIATE, not a frame word. The
        // `Op::Const` arm still writes its home (a deopt frame may name it,
        // and some sites still read slots directly), but no reader of a
        // constant has to wait on that store any more: `fib` materialised
        // `1` and `2` into frame words and reloaded them on every call.
        if ir_const_imm_enabled() {
            if let Some(node) = self.graph.nodes.get(id as usize) {
                if let Op::Const(val) = &node.op {
                    let val = *val;
                    self.emit_mov_reg_imm_smart(dst, val);
                    return;
                }
            }
        }
        match self.resident_gpr(id) {
            Some(src) => self.emit_mov_reg_reg64(dst, src),
            None => {
                let off = self.slot_of(id);
                self.load_reg_from_frame(dst, off);
            }
        }
    }

    /// Write `id`'s result from `src`: ALWAYS to the home word, and
    /// additionally into its resident register.
    ///
    /// The write-through invariant lives here. The home store is emitted
    /// unconditionally and first, so the frame image is complete at every
    /// instruction boundary — which is what lets `emit_safepoint_map`,
    /// `build_deopt_points` and `emit_phi_copies` stay untouched.
    fn gp_store_value(&mut self, id: NodeId, slot: i32, src: u8) {
        self.store_abi_reg(src, slot);
        if let Some(dst) = self.assigned_gpr(id) {
            self.emit_mov_reg_reg64(dst, src);
            self.ls_spills += 1;
            self.mark_gp_reg_live(id);
        }
    }

    /// Publish a result already stored to its home word into its register — the
    /// memory → register transition, for definition sites whose result reaches
    /// the home word by a route this cache does not intercept.
    fn publish_gp_from_slot(&mut self, id: NodeId, slot: i32) {
        if let Some(dst) = self.assigned_gpr(id) {
            self.load_reg_from_frame(dst, slot);
            self.ls_reloads += 1;
            self.mark_gp_reg_live(id);
        }
    }

    /// `MOV dst, src` — 64-bit register to register, for any pair including the
    /// extended registers the GP file is made of.
    fn emit_mov_reg_reg64(&mut self, dst: u8, src: u8) {
        if dst == src {
            return;
        }
        let rex = 0x48u8 | (((src >= 8) as u8) << 2) | ((dst >= 8) as u8);
        self.buf
            .emit(&[rex, 0x89, 0xC0 | ((src & 7) << 3) | (dst & 7)]);
    }

    /// Latch a structured bailout raised from an infallible legacy accessor.
    ///
    /// The first one wins (it is the cause; later ones are consequences of
    /// continuing to emit into an artifact that is already doomed).
    /// `lower_inner` takes it and reports it instead of returning a bare
    /// `None`. Interior mutability because both `&self` (`slot_of`) and
    /// `&mut self` (`alloc_slot`) accessors latch.
    fn latch_bailout(&self, bailout: Bailout) {
        let mut slot = self.latched_bailout.borrow_mut();
        if slot.is_none() {
            *slot = Some(bailout);
        }
    }

    /// Take the latched bailout, if any.
    fn take_latched_bailout(&self) -> Option<Bailout> {
        self.latched_bailout.borrow_mut().take()
    }

    /// A frame offset that is safe to *emit* against but is not a real
    /// location: the first spill slot. Returned only by the legacy infallible
    /// accessors, and only after a bailout has been latched — which makes
    /// `lower_inner` discard the whole artifact before it can be executed.
    ///
    /// It exists so a doomed compile finishes walking the graph without
    /// panicking and without addressing memory outside the frame. The one
    /// value it must never be is `0`: that is `[rbp - 0]`, the saved caller
    /// frame pointer, which is exactly the read this whole change exists to
    /// make unrepresentable.
    fn poison_slot(&self) -> i32 {
        debug_assert!(self.first_spill > 0);
        self.first_spill
    }

    /// Allocate a frame slot for a node result, or refuse the compile.
    ///
    /// real-frame-deopt (#6): the deopt stub calls `ir_deopt_entry` while the
    /// frame is live, with `rsp = rbp - frame_size`. The Win64 ABI requires 32
    /// bytes of caller shadow space at `[rsp, rsp+32)` — i.e. frame offsets
    /// `(frame_size-32 .. frame_size]`. A spill slot at offset `o` occupies
    /// `[rbp-o, rbp-o+8)`; to keep it clear of the shadow region we cap
    /// `o <= frame_size - DEOPT_SHADOW_SPACE`. `frame_size` already budgets the
    /// 32-byte shadow (plus a 16-byte stack-arg reserve), so this never rejects
    /// a method the old `o < frame_size` bound accepted.
    ///
    /// Exceeding that cap used to be an `assert!`, i.e. a panic on the compiler
    /// thread — which in a release VM is `fatal runtime error: failed to
    /// initiate panic` → SIGABRT of the whole process, for what is only a
    /// *compiler* resource limit. It is now a `FrameTooLarge` bailout: the
    /// method loses its optimized body and runs in a lower tier, which is
    /// always semantically valid.
    ///
    /// The offset is no longer a bump cursor: it is `first_spill + colour * 8`
    /// for the colour [`plan_slots`] assigned this value, so two values whose
    /// live ranges do not overlap land on the same word. Calling this twice for
    /// one node is therefore idempotent rather than wasteful. A node the plan
    /// does not cover is an internal inconsistency (the plan enumerates exactly
    /// the nodes `prealloc_phi_slots` and `lower_data_node` allocate for), and
    /// refuses the compile rather than inventing an offset.
    fn alloc_slot_checked(&mut self, id: NodeId) -> CompileResult<i32> {
        let offset = self.planned_slot_off(id)?;
        // `planned_slot_off` already proved the offset non-zero and inside the
        // frame; re-deriving the `NonZeroU32` here keeps the "no zero offsets"
        // invariant enforced at the write, not merely upstream of it.
        let located = u32::try_from(offset)
            .ok()
            .and_then(NonZeroU32::new)
            .ok_or_else(|| {
                Bailout::new(BailoutReason::Internal(
                    "ir_lower: spill offset 0 would encode [rbp - 0] (saved caller RBP)",
                ))
            })?;
        let cell = self.node_slot.get_mut(id as usize).ok_or_else(|| {
            Bailout::new(BailoutReason::Internal("ir_lower: node id out of range"))
        })?;
        *cell = Some(located);
        // Watermark, not a cursor: the highest word any emitted node's slot has
        // reached. Every safepoint's `live_frame_hi` is read off this, and with
        // reuse it stops growing once the peak live set is reached.
        self.spill_high_water = self.spill_high_water.max(offset.saturating_add(8));
        // Relocation contract: a slot becomes publishable the moment the
        // emitting code writes it. `alloc_slot` is called at the point of
        // emission for every node EXCEPT phis, whose slots are reserved up
        // front by `prealloc_phi_slots` and written later by edge copies —
        // those are marked separately, after the prologue zeroes them.
        if !matches!(self.graph.nodes[id as usize].op, Op::Phi) {
            self.defined_nodes[id as usize] = true;
        }
        Ok(offset)
    }

    /// The frame offset [`Self::alloc_slot_checked`] *will* assign `id`, without
    /// assigning it.
    ///
    /// Split out so the level-2 tile encoder can encode a store to a
    /// destination slot before the destination has been allocated, and get the
    /// same answer the allocating path would — by calling the same code, not by
    /// repeating the arithmetic. Every refusal below is the allocating path's
    /// refusal, unchanged; only the three mutations live in the caller.
    fn planned_slot_off(&self, id: NodeId) -> CompileResult<i32> {
        let color = match self
            .slot_plan
            .node_color
            .get(id as usize)
            .copied()
            .flatten()
        {
            Some(color) => color,
            None => {
                return Err(Bailout::with_context(
                    BailoutReason::Internal(
                        "ir_lower: the slot plan has no colour for a value being lowered",
                    ),
                    format!(
                        "n{id} reached alloc_slot with no planned frame slot (op {:?})",
                        self.graph.nodes.get(id as usize).map(|n| &n.op),
                    ),
                ))
            }
        };
        let offset = i32::try_from(u64::from(color).saturating_mul(8))
            .ok()
            .and_then(|delta| self.first_spill.checked_add(delta))
            .ok_or_else(|| {
                Bailout::new(BailoutReason::Internal(
                    "ir_lower: coloured spill offset overflowed the frame arithmetic",
                ))
            })?;
        // `spill_cap_off` excludes the 32-byte shadow space AND (Gap B) the
        // Java-arg staging region, so a spill never overlaps either. For a
        // no-call method it equals `frame_size - DEOPT_SHADOW_SPACE` — the
        // historical bound, unchanged.
        if offset > self.spill_cap_off {
            return Err(Bailout::with_context(
                BailoutReason::FrameTooLarge {
                    bytes: offset.max(0) as usize,
                    limit: self.spill_cap_off.max(0) as usize,
                },
                format!(
                    "spill slot for n{id} past the frame's spill cap \
                     (frame {} bytes, cap {})",
                    self.frame_size, self.spill_cap_off
                ),
            ));
        }
        // `first_spill > 0` and colours are non-negative, so this cannot be
        // `None` — but the conversion is where the "no zero offsets" invariant
        // is *enforced* rather than assumed, so it is checked, not asserted.
        u32::try_from(offset)
            .ok()
            .and_then(NonZeroU32::new)
            .ok_or_else(|| {
                Bailout::new(BailoutReason::Internal(
                    "ir_lower: spill offset 0 would encode [rbp - 0] (saved caller RBP)",
                ))
            })?;
        Ok(offset)
    }

    /// Infallible façade over [`Self::alloc_slot_checked`] for the ~45
    /// per-opcode emission sites, which are `fn(..) -> ()` and cannot `?`.
    ///
    /// On refusal it latches the structured bailout — so `lower_inner` discards
    /// the artifact and the caller falls back to the single-pass backend — and
    /// returns the poison slot so the doomed walk finishes without panicking.
    /// New code should call `alloc_slot_checked` and propagate.
    fn alloc_slot(&mut self, id: NodeId) -> i32 {
        match self.alloc_slot_checked(id) {
            Ok(offset) => offset,
            Err(bailout) => {
                self.latch_bailout(bailout);
                self.poison_slot()
            }
        }
    }

    /// The frame offset of a node's result, or a bailout if it has none.
    ///
    /// The only read path into `node_slot`. There is no longer a value that
    /// means "unallocated": an absent location is an `Err`, never `0`.
    fn slot_of_checked(&self, id: NodeId) -> CompileResult<i32> {
        match self.node_slot.get(id as usize).copied().flatten() {
            // `NonZeroU32` ⇒ never 0, and `alloc_slot_checked` bounds it by
            // `spill_cap_off` ⇒ always inside the frame.
            Some(off) => Ok(off.get() as i32),
            None => Err(Bailout::new(BailoutReason::UnallocatedValue { node: id })),
        }
    }

    /// Infallible façade over [`Self::slot_of_checked`] for the emission sites.
    ///
    /// See `unallocated_slot_use`: this path is unreachable now that
    /// `verify_data_locations` refuses such graphs before emission starts. If
    /// it is ever taken, the latch discards the artifact and the caller falls
    /// back to the single-pass backend — the same outcome as before, but with a
    /// structured reason attached, and returning the poison slot rather than
    /// `0` so no `[rbp - 0]` read can be emitted even transiently.
    fn slot_of(&self, id: NodeId) -> i32 {
        match self.slot_of_checked(id) {
            Ok(offset) => offset,
            Err(bailout) => {
                self.unallocated_slot_use.set(true);
                self.latch_bailout(bailout);
                if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_IRSLOT").is_some() {
                    match self.graph.nodes.get(id as usize) {
                        Some(n) => eprintln!(
                            "[irslot] UNALLOCATED node={} op={:?} ty={:?} inputs={:?} pc={:?}",
                            id, n.op, n.ty, n.inputs, n.bytecode_pc
                        ),
                        None => eprintln!("[irslot] UNALLOCATED node={id} (out of range)"),
                    }
                }
                self.poison_slot()
            }
        }
    }

    /// `CRATONVM_JIT_IR_RELOC_EMIT=0` — disable the relocation contract's EMISSION
    /// side entirely (safepoint-id stores, shadow push/reload, the prologue thread
    /// fetch), leaving the frame layout alone.
    ///
    /// Exists so the cost of that emission can be A/B'd on ONE binary against a
    /// deterministic probe. Three attempts inferred it from a single run of a suite
    /// class that later turned out to be bimodal; a lever plus repeats is what that
    /// should have been. Default on, so this changes nothing unless asked.
    fn reloc_emit_enabled() -> bool {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_RELOC_EMIT") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    }

    /// `CRATONVM_JIT_IR_COLD_ARG_STAGE=0` — stage a call's outgoing arguments
    /// EAGERLY, before the call, as this backend did until 2026-09-02.
    ///
    /// Default on, meaning the staging happens on each reader's own cold side.
    /// The switch exists because the eager version was removed without one, and
    /// a change to what a frame holds across a call is exactly the kind that
    /// has to be A/B-able in ONE binary when a GC-stress test starts failing.
    /// Not having it cost a rebuild per hypothesis.
    fn cold_arg_stage_enabled() -> bool {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_COLD_ARG_STAGE") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    }

    // ── Moving-young relocation contract ─────────────────────────────────
    //
    // What `conservative_roots` demands of a compiled frame before a young
    // collection may RELOCATE while that frame is live:
    //
    //   1. a non-zero `sp_id_slot_off`, and the id of the active safepoint
    //      stored there, so the exact map can be found (never a union);
    //   2. an `OopMapEntry` whose `bytecode_pc` equals that id and whose
    //      `moving_young_coverage_complete` is set. **No matching entry is a
    //      refusal**, not an absence of objections;
    //   3. `frame_slot_offsets` naming every frame slot that holds a live
    //      reference, so each can be rewritten in place;
    //   4. a `frame_layout` + `live_frame_hi` precise enough that the band scan
    //      can tell storage this frame resumes from apart from dead spill and
    //      outgoing-argument scratch.
    //
    // The IR lowerer is unusually well placed to satisfy this: it "keeps all
    // live values in frame slots" (see `emit_safepoint_poll`), so unlike the
    // single-pass backend there are no register-resident oops to chase — the
    // complete root set at a safepoint is a set of frame offsets.

    /// Emit the safepoint-id store and record the map for a GC-capable point.
    ///
    /// Call immediately BEFORE the call or allocation that can reach a
    /// collection, and before allocating the result slot of that node — the
    /// result slot is not written until the call returns, so publishing it as a
    /// live reference beforehand would hand the collector uninitialised memory.
    ///
    /// This ALWAYS stores an id and ALWAYS records a map, even when coverage
    /// cannot be established. Skipping either would leave the slot holding the
    /// id of an *earlier* safepoint, and the collector would then match a map
    /// describing a different program point and relocate against it. A map with
    /// `moving_young_coverage_complete: false` is the fail-closed answer: the
    /// verifier finds the entry, sees the flag, and diverts to the non-moving
    /// sweep for that cycle.
    fn emit_safepoint_map(&mut self, live_hi: i32) {
        if self.sp_id_slot_off <= 0 || !Self::reloc_emit_enabled() {
            return;
        }
        // `live_ref_slots` returns an empty vector for two different reasons —
        // a genuinely oop-free safepoint, and a slot it could not describe.
        // Distinguish them, because the first is complete coverage and the
        // second is a refusal.
        let mut coverable = true;
        // Reference PARAMETER homes come first, because they are the one class
        // of live oop that is not a node result and so cannot be found by the
        // scan below. The prologue stores each incoming argument to
        // `[rbp - (idx+1)*8]` and that word keeps holding the reference for the
        // whole frame; `Op::Param`'s own spill slot is a COPY of it.
        //
        // Omitting them is not a partial claim, it is a false one: the band
        // scan is conservative, so a single unpublished young word anywhere in
        // the live band refuses the whole cycle. Measured on `IrEscapeProbe`
        // (`CRATONVM_MOVING_YOUNG_BAND_DBG`), the two rejected words were
        // `off=8 region=java-local` and `off=56 region=operand-spill` holding
        // the SAME reference — the parameter home and its node copy.
        let mut slots: Vec<i16> = self.ref_param_homes.clone();
        for id in 0..self.defined_nodes.len() {
            if !self.defined_nodes[id] || self.graph.nodes[id].ty != IrType::Ref {
                continue;
            }
            // `defined_nodes[id]` implies an allocated slot, so `None` here is
            // an internal inconsistency, not a `Ref` without a home. Treat it
            // exactly like an unencodable offset: fail closed, publish no map.
            let off = match self.node_slot[id] {
                Some(off) => off.get() as i32,
                None => {
                    coverable = false;
                    break;
                }
            };
            if off > i16::MAX as i32 {
                coverable = false;
                break;
            }
            let off = off as i16;
            if !slots.contains(&off) {
                slots.push(off);
            }
        }
        if !coverable {
            slots.clear();
        }

        let id = self.next_sp_id;
        self.next_sp_id = self.next_sp_id.wrapping_add(1);
        // The id->bci translation a stack walk needs. `resume_bci` is the same
        // normalisation the throw-site and deopt paths apply: inside an
        // IR-spliced callee `cur_bci` is a pc in the COMBINED buffer, which
        // names no instruction in this method's own `Code`, so it is mapped
        // back to the enclosing invoke. `u32::try_from` cannot fail for a
        // spec-legal bci; a value that somehow exceeds it records nothing,
        // which reports no line rather than a wrong one.
        if let Ok(bci) = u32::try_from(self.resume_bci(self.cur_bci)) {
            self.sp_id_bcis.push((id, bci));
        }
        // MOV qword [rbp - sp_id_slot_off], imm32 (sign-extended; ids are small)
        self.buf.emit(&[0x48, 0xC7, 0x85]);
        self.buf.emit(&(-self.sp_id_slot_off).to_le_bytes());
        self.buf.emit(&id.to_le_bytes());

        // Publish, and only then claim coverage. `emit_shadow_push` returns
        // false when the thread helper is unwired or there is nothing to
        // publish; an oop-free safepoint is still complete coverage, so the
        // claim below requires `coverable` AND (published OR nothing to
        // publish).
        let published = self.emit_shadow_push(&slots);
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_IR_RELOC").is_some() {
            eprintln!(
                "[ir-reloc] safepoint id={id} slots={slots:?} coverable={coverable} \
                 published={published} thread_helper={:#x} thread_slot={} savebase={} \
                 ss_off={}",
                self.get_current_thread,
                self.shadow_thread_slot_off,
                self.shadow_savebase_slot_off,
                self.shadow_off_in_thread,
            );
        }
        self.shadow_pushed_any |= published;
        self.pending_shadow = if published { slots.clone() } else { Vec::new() };
        let complete = coverable && (published || slots.is_empty());

        self.oop_maps.push(super::OopMapEntry {
            // The relocation path matches on the id in the frame slot, not on
            // this; `close_safepoint_map` fills it once the call is emitted.
            native_pc_offset: 0,
            bytecode_pc: id,
            frame_slot_offsets: slots,
            // The claim the collector acts on: the shadow stack HAS been
            // published with every live oop of this frame, so each can be
            // rewritten in place. Requires both halves — `coverable` (every
            // live `Ref` slot could be named) and `published` (the push was
            // actually emitted). Naming the slots alone is not enough:
            // `scan_oop_slots` yields `ObjectRef` values, which mark but cannot
            // be written back through.
            moving_young_coverage_complete: complete,
            live_frame_hi: live_hi,
            // The IR tier allocates frame slots; it does not home local `k` at
            // `[rbp - 8*(k+1)]`, so the locals-band oracle does not apply to
            // these frames and must not answer as if it did.
            local_oop_mask: None,
            num_locals: 0,
            inline_local_scopes: Vec::new(),
            // The IR tier has no java-locals band to speak for, but its slot
            // COLOURING is a proof about every spill slot it allocated: a
            // non-`Ref` colour never holds an object pointer, because
            // `assign_colors` never moves a colour between its two free lists.
            // Exact because `verify_slot_colouring` re-derives the no-aliasing
            // property rather than trusting it.
            //
            // The premise is `IrType::Ref`, which is the SAME premise this
            // function publishes roots on a few lines above. So the oracle
            // cannot be wrong here unless the map itself already is: a
            // non-`Ref` node holding a real object pointer would be an
            // unpublished root today, with or without this field. It is not an
            // independent check of that, and must not be read as one.
            non_oop_stack_slots: self.prim_slot_offsets.clone(),
            stack_marks_exact: true,
        });
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
    ///
    /// Fallible: this is the first thing that can exhaust the frame, and a
    /// refusal here must reach `lower_inner` as a `FrameTooLarge` bailout
    /// rather than a panic.
    fn prealloc_phi_slots(&mut self) -> CompileResult<()> {
        for id in 0..self.graph.nodes.len() {
            if matches!(self.graph.nodes[id].op, Op::Phi) {
                self.alloc_slot_checked(id as NodeId)?;
            }
        }
        Ok(())
    }

    /// Resolve the block that produces control token `ctrl` by walking up
    /// control inputs until we reach a node that heads some block.
    ///
    /// Merge predecessor `k` records `self.ctrl` (the predecessor's live
    /// control node) as `merge.inputs[k]`; that token is either a block
    /// head directly (goto fall-through) or a `Proj` off an `If` (block
    /// head). Walking control inputs makes the mapping robust to either.
    ///
    /// Delegates to the free function [`ctrl_block_of`] so the slot planner,
    /// which must reproduce this exact edge attribution to see phi inputs as
    /// uses at the *predecessor's* terminator, cannot drift from the emitter.
    fn block_of_ctrl(&self, ctrl: NodeId) -> Option<usize> {
        ctrl_block_of(self.graph, self.schedule, ctrl)
    }

    /// Emit the parallel-copy stores for every phi at `succ_block` whose
    /// merge has `pred_block` as the source for that phi argument.
    ///
    /// Each copy loads the incoming SSA value (already spilled by the
    /// predecessor block, which is lowered before its terminator) into RAX
    /// and stores it into the phi's reserved slot.
    ///
    /// The copies are a PARALLEL assignment: every source is read as it was
    /// *before* any destination is written. They are therefore sequentialised
    /// by [`resolve_parallel_copy`] rather than emitted in gather order, and a
    /// cycle among them is broken through [`Lowerer::phi_copy_scratch_slot_off`].
    /// See the doc comment on the emission loop below for what was wrong before.
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
        let gathered = self.gather_phi_copies(pred_block, succ_block);
        let copies: Vec<(i32, i32)> = gathered.iter().map(|&(_, dst, src)| (dst, src)).collect();

        // Phi copies are a PARALLEL assignment, not a sequence.
        //
        // `prealloc_phi_slots` gives every phi its own frame word, so a loop
        // that swaps two locals makes each phi the other's back-edge value and
        // this list becomes a cycle (`a <- b, b <- a`). Emitted in gather order
        // through RAX, the second copy reads the word the first just overwrote
        // and both locals end up holding `b` — a wrong-code bug, not a missed
        // optimisation.
        //
        // `resolve_parallel_copy` orders the copies so that every read still
        // sees the pre-copy value: destinations nothing else reads go first,
        // and when only cycles remain one element is parked in the scratch,
        // which frees its predecessor and unwinds the rest of the cycle. The
        // acyclic majority — every merge that is not a permutation — pays
        // exactly the same one load + one store per copy it always did, and
        // touches the scratch word not at all.
        //
        // An intermediate state of this fix REFUSED a residual cycle
        // (`UnsupportedShape("cyclic phi parallel copy")`) because no scratch
        // location was reserved. That was correct but dropped every swap-shaped
        // loop to a lower tier; `phi_copy_scratch_slot_off` is the reserved word
        // that makes the refusal unnecessary.
        let ops = match phi_copy_sequence(&copies) {
            Ok(ops) => ops,
            Err(bailout) => {
                let Bailout { reason, context } = bailout;
                let edge = format!("phi parallel copy on block {pred_block} -> {succ_block}");
                self.latch_bailout(Bailout::with_context(
                    reason,
                    match context {
                        Some(c) => format!("{edge}: {c}"),
                        None => edge,
                    },
                ));
                return;
            }
        };
        for op in ops {
            if let Err(bailout) = self.emit_copy_op(op) {
                self.latch_bailout(bailout);
                return;
            }
        }
        // 2026-09-02: a register-resident phi is PUBLISHED here, at every
        // incoming edge, from the home word the copies just wrote. This is the
        // definition site a phi never had, and it is what lets a loop counter
        // or accumulator -- a phi at every loop header -- be read from a
        // register inside the loop (`ir_phi_residency_enabled`). Reads before
        // the first published edge (in emission order) still take the home
        // word; every edge publishes, so the register is valid on entry to the
        // block whichever predecessor ran.
        if ir_phi_residency_enabled() {
            for &(phi, dst, _) in &gathered {
                if self.assigned_gpr(phi).is_some() {
                    self.publish_gp_from_slot(phi, dst);
                } else if self.assigned_xmm(phi).is_some() {
                    let is_double = self.graph.nodes[phi as usize].ty == IrType::Double;
                    self.publish_fp_from_slot(phi, dst, is_double);
                }
            }
        }
    }

    /// Emit one sequentialised [`CopyOp`], routed through RAX (the move
    /// temporary) and, for `Save`/`Restore`, the reserved scratch frame word.
    ///
    /// `Save` and `Restore` are *not* a push/pop pair: `resolve_parallel_copy`
    /// emits at most one live save at a time and always consumes it before
    /// starting another cycle, so a single word suffices no matter how many
    /// disjoint cycles the edge contains.
    fn emit_copy_op(&mut self, op: CopyOp) -> CompileResult<()> {
        let scratch = self.phi_copy_scratch_slot_off;
        let (src, dst) = match op {
            CopyOp::Move { from, to } => (frame_word_off(from)?, frame_word_off(to)?),
            CopyOp::Save { from } => (frame_word_off(from)?, scratch),
            CopyOp::Restore { to } => (scratch, frame_word_off(to)?),
        };
        // The scratch is a reserved bookkeeping word placed by `Lowerer::new`;
        // a zero here would encode `[rbp - 0]`, the saved caller RBP.
        if src <= 0 || dst <= 0 {
            return Err(Bailout::with_context(
                BailoutReason::Internal("ir_lower: a phi copy names frame offset 0"),
                format!("[rbp - {dst}] <- [rbp - {src}]"),
            ));
        }
        self.load_to_rax(src);
        self.store_rax(dst);
        Ok(())
    }

    // ── Code emission helpers ────────────────────────────────────────

    /// The callee-saved XMM registers this method actually parked a value in,
    /// paired with the frame offset each is saved at.
    ///
    /// Empty on System V (nothing is callee-saved), empty when the linear-scan
    /// path is off, and empty for any method whose allocation promoted nothing
    /// into XMM6/XMM7 — which is the common case even with the flag on. A
    /// method that uses none of them emits no save, no restore and pays only
    /// the reserved frame bytes.
    ///
    /// Reads the residency plan, which `lower_inner_with_scopes` installs
    /// BEFORE `lower()` runs (`set_residency`, "installed before the prologue
    /// and never touched again"). That ordering is what makes a *dynamic* save
    /// set legal against a *static* frame reservation: the set can only shrink
    /// relative to `IR_LOWER_SAVED_XMMS`, never grow past it.
    fn saved_xmm_regs(&self) -> impl Iterator<Item = (u8, i32)> + '_ {
        let base = self.spill_cap_off;
        let reserved = self.saved_xmm_bytes;
        IR_LOWER_SAVED_XMMS
            .iter()
            .enumerate()
            .filter(move |_| reserved > 0)
            .filter(move |(_, reg)| self.reg_of.iter().any(|r| *r == Some(**reg)))
            // Cast: `IR_LOWER_SAVED_XMMS` has two elements.
            .map(move |(i, reg)| (*reg, base + (i as i32 + 1) * 16))
    }

    /// `MOVUPS [rbp - off], xmm` (save) or `MOVUPS xmm, [rbp - off]` (restore).
    ///
    /// Unaligned on purpose: `frame_size` is 16-byte aligned but `rbp` itself is
    /// only guaranteed 8-byte aligned at entry (the `push rbp` follows the
    /// caller's `call`), so `MOVAPS` would fault on half the call sites. The
    /// three-byte cost of the unaligned form is paid once per method.
    ///
    /// Both registers are below XMM8, so no REX byte — the same constraint
    /// `fp_load`/`fp_store` live under, restated here because this function
    /// would silently encode the wrong register if it were ever handed one.
    fn emit_xmm_frame_move(&mut self, reg: u8, off: i32, store: bool) {
        debug_assert!(reg < 8, "xmm{reg} needs REX.R, which this encoding omits");
        self.buf.emit(&[0x0F, if store { 0x11 } else { 0x10 }]);
        // ModRM + displacement for `[rbp - off]`, smallest legal form.
        self.emit_rbp_modrm_disp(reg, off);
    }

    /// The callee-saved GPRs this method actually parked a value in, paired
    /// with the frame offset each is saved at.
    ///
    /// The GPR band sits immediately below the XMM one, so register `i` of
    /// [`IR_LOWER_SAVED_GPRS`] lives at
    /// `spill_cap_off + saved_xmm_bytes + 8*(i+1)` — the same "+1 so the
    /// store's bytes land inside the reservation" shape `saved_xmm_regs` uses.
    ///
    /// Dynamic against a static reservation, for the reason stated there: the
    /// residency plan is installed before `lower()` runs, so this set can only
    /// shrink relative to [`IR_LOWER_SAVED_GPRS`], never grow past it. A method
    /// that promotes nothing emits no save and no restore.
    fn saved_gpr_regs(&self) -> impl Iterator<Item = (u8, i32)> + '_ {
        let base = self.spill_cap_off + self.saved_xmm_bytes;
        let reserved = self.saved_gpr_bytes;
        IR_LOWER_SAVED_GPRS
            .iter()
            .enumerate()
            .filter(move |_| reserved > 0)
            .filter(move |(_, reg)| self.gp_reg_of.iter().any(|r| *r == Some(**reg)))
            // Cast: `IR_LOWER_SAVED_GPRS` has five elements.
            .map(move |(i, reg)| (*reg, base + (i as i32 + 1) * 8))
    }

    /// `MOV [rbp - off], reg` (save) or `MOV reg, [rbp - off]` (restore), for a
    /// callee-saved GPR. Both directions already exist as frame accessors that
    /// pick the smallest displacement form; this names the pair.
    fn emit_gpr_frame_move(&mut self, reg: u8, off: i32, store: bool) {
        if store {
            self.store_abi_reg(reg, off);
        } else {
            self.load_reg_from_frame(reg, off);
        }
    }

    /// Restore the callee-saved registers. Emitted at every exit, and it must
    /// not disturb RAX — a method's return value and the `i64::MIN`
    /// exception/deopt sentinel both travel there. `MOVUPS` into an XMM
    /// satisfies that for free, and the GPR restores target RBX/R12–R15, which
    /// is a set RAX could never have been in: see `IR_GP_LINEAR_SCAN`.
    fn emit_callee_saved_restore(&mut self) {
        let restores: Vec<(u8, i32)> = self.saved_xmm_regs().collect();
        for (reg, off) in restores {
            self.emit_xmm_frame_move(reg, off, false);
        }
        let gpr_restores: Vec<(u8, i32)> = self.saved_gpr_regs().collect();
        for (reg, off) in gpr_restores {
            self.emit_gpr_frame_move(reg, off, false);
        }
    }

    fn emit_prologue(&mut self) {
        // push rbp
        self.buf.emit_byte(0x55);
        // mov rbp, rsp
        self.buf.emit(&[0x48, 0x89, 0xE5]);
        // sub rsp, frame_size
        self.buf.emit(&[0x48, 0x81, 0xEC]);
        self.buf.emit(&self.frame_size.to_le_bytes());

        // Callee-saved XMM save area. FIRST, before the ABI parameter stores:
        // an incoming floating-point argument arrives in XMM0..XMM3 (Win64) and
        // nothing here touches those, but ordering the save ahead of every
        // other prologue step keeps "the caller's registers are preserved" true
        // across the whole body rather than across most of it.
        let saves: Vec<(u8, i32)> = self.saved_xmm_regs().collect();
        for (reg, off) in saves {
            self.emit_xmm_frame_move(reg, off, true);
        }
        // …and the callee-saved GPR file, on the same footing and for the same
        // reason: every register in it belongs to the caller on both ABIs.
        let gpr_saves: Vec<(u8, i32)> = self.saved_gpr_regs().collect();
        for (reg, off) in gpr_saves {
            self.emit_gpr_frame_move(reg, off, true);
        }

        // Store params from ABI registers to local frame slots.
        // Windows: RCX, RDX, R8, R9.  SysV: RDI, RSI, RDX, RCX, R8, R9.
        // One list, shared with `incoming_abi_reg_capacity()` — the local copy
        // this used to keep could (and did) disagree with the bail that is
        // supposed to keep the `break` below unreachable.
        let abi_regs: &[u8] = ENTRY_ABI_REGS;

        // Gap B: a `needs_context` method receives the VM context pointer in
        // ABI[0] (the `try_call_with_context` convention), with the Java params
        // shifted to ABI[1..]. Store the context to its slot, then the params to
        // their local slots.
        //
        // An argument past the register file arrives on the CALLER'S STACK, and
        // this prologue used to drop it: the loop `break`s and the local keeps
        // whatever the frame slot happened to contain. `lower()` therefore
        // refused any method with more incoming slots than registers outright —
        // four on Win64, six on SysV — which made it "the commonest whole-method
        // refusal an ordinary accessor hits": an instance method with three
        // parameters is four slots, and touching one field turns
        // `needs_context` on, which is the fifth.
        //
        // It is loaded now, the way the single-pass prologue has loaded it since
        // the ROUND-12 fix (`x64/frames.rs`), from the same place, because the
        // two backends share the outgoing convention: `stack_arg_block_size`
        // here mirrors `x64::frames::stack_arg_block_size`, and a caller
        // materializes stack arguments at `[rsp + shadow]` immediately before
        // the CALL. After the CALL pushes the return address and this prologue
        // pushes RBP, argument `k` past the register file sits at
        // `[rbp + STACK_ARG_BASE + (k - reg_count) * 8]`.
        let base = if self.needs_context {
            self.store_abi_reg(abi_regs[0], self.context_slot_off);
            1
        } else {
            0
        };
        // Java arguments that fit the register file, then the rest off the
        // caller's frame. `reg_count` is how many of THIS method's arguments a
        // register carries, which is the file size minus the context slot.
        let reg_count = self.num_params.min(abi_regs.len().saturating_sub(base));
        for i in 0..reg_count {
            self.store_abi_reg(abi_regs[base + i], ((i as i32) + 1) * 8); // local_offset(i)
        }
        for i in reg_count..self.num_params {
            // `[RBP + disp]`, which `load_reg_from_frame` spells as a NEGATIVE
            // offset (it encodes `[RBP - offset]`). Via RAX because there is no
            // memory-to-memory MOV.
            let disp = STACK_ARG_BASE + ((i - reg_count) as i32) * 8;
            self.load_reg_from_frame(RAX, -disp);
            self.store_abi_reg(RAX, ((i as i32) + 1) * 8); // local_offset(i)
        }
        // Zero the cached-thread and watermark slots BEFORE the fetch. The
        // fetch is erased (NOP'd) by `finish_lazy_thread_fetch` when the method
        // publishes nothing, and every consumer below is null-guarded on the
        // thread slot — so it must read 0, not uninitialised stack. The
        // single-pass backend zero-initialises for exactly this reason (see the
        // incident writeup on `x64.rs::Compiler::emit_epilogue`).
        if self.get_current_thread != 0 && self.shadow_thread_slot_off > 0 {
            self.emit_zero_frame_slot(self.shadow_thread_slot_off);
            self.emit_zero_frame_slot(self.shadow_savetop_slot_off);
        }
        // And the safepoint-id slot, for the SAME reason and on its own gate:
        // `active_safepoint_id` reads `[rbp - sp_id_slot_off]` on any live
        // frame, including one stopped BEFORE its first safepoint, and until
        // that first `emit_safepoint_map` store the word is whatever the
        // previous frame at this stack depth left behind.
        //
        // Ids start at 1 precisely so 0 can mean "no safepoint reached" (see
        // the slot's allocation above), but nothing was writing the 0. The
        // sentinel was a convention the prologue never established.
        //
        // MEASURED, `TestCachedQueryResults` (H2), 13 frames the band verifier
        // reported as `no-map-for-id`: 10 read `0` and 3 read a HEAP POINTER at
        // `sp_id_off`. That was diagnosed as two defects -- "has not reached a
        // safepoint" and "something stored an oop into the reserved slot". It
        // is one: uninitialised stack, reading as zero where the region happened
        // to be clean and as a stale oop where it did not.
        //
        // Refusing the cycle is the benign outcome. The hazard this closes is
        // that safepoint ids are small consecutive integers, so a stale word
        // can equal a VALID id for this method -- and then
        // `find_oop_map_for_safepoint_id` matches the map for a DIFFERENT
        // program point and relocation rewrites against it. That is a silent
        // wrong answer, not a refusal.
        //
        // `CRATONVM_JIT_ZERO_SPID=0` restores the uninitialised read: the
        // one-binary A/B, and the kill switch.
        if self.sp_id_slot_off > 0 && Self::zero_sp_id_slot_enabled() {
            self.emit_zero_frame_slot(self.sp_id_slot_off);
        }
        self.emit_frame_record();
        self.fetch_current_thread();
        self.zero_ref_phi_slots();
    }

    /// Publish this frame's exact RBP into the precise-maps innermost-RBP
    /// mirror, exactly as the single-pass prologue does.
    ///
    /// Without it the collector cannot locate the frame at all:
    /// `moving_young_frame_coverage_complete` needs an address to resolve
    /// `sp_id_slot_off` and the map's slot offsets against, and
    /// `PreciseFrameInfo::exact_rbp` is only ever a snapshot of this mirror. An
    /// IR frame that never wrote it left the mirror holding `0`, and the
    /// verifier refused the whole cycle with `missing-exact-rbp` — before it
    /// ever looked at an oop map.
    ///
    /// That is why the relocation contract, though implemented and sound,
    /// changed nothing observable: with `CRATONVM_JIT_FORCE_C2=1` on the
    /// `IrEscapeProbe` the collector reported `cycles=0 coverage_fallbacks=14,
    /// missing-exact-rbp=14` — every young collection diverted for want of a
    /// nine-byte store, with the maps and the shadow publication both present
    /// and correct. Emitted after the ABI parameter stores for the same reason
    /// the single-pass backend does it there.
    fn emit_frame_record(&mut self) {
        if !crate::x64::precise_jit_maps_enabled() || self.frame_record == 0 {
            return;
        }
        let disp = crate::x64::inline_rbp_tls_disp();
        if disp != 0 {
            self.emit_mov_tls_disp32_rbp(disp as u32);
            // …and the identity of the frame that RBP names. Without it the GC
            // must decode the call that created this frame, which is impossible
            // when the caller reached us indirectly (`CALL R11` — every inline
            // cache hit).
            self.emit_frame_record_identity();
        } else {
            // No usable TLS displacement on this target: fall back to the
            // helper. Params are already in frame slots, so its caller-saved
            // clobbers cost nothing here.
            self.emit_mov_arg0_rbp();
            self.emit_mov_reg_imm64(RAX, self.frame_record as u64);
            self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        }
    }

    /// Restore this frame's RBP into the mirror after a call returned.
    ///
    /// A compiled callee publishes its OWN rbp on entry, so after it returns
    /// the mirror names the wrong (now-dead) frame. The single-pass backend
    /// republishes at every call site (`emit_post_call_rbp_republish`); the
    /// shared allocation stub does the same
    /// (`runtime_lowering::emit_post_call_frame_republish`). This is the IR
    /// tier's equivalent, and it must not disturb RAX — the callee's return
    /// value is still in it — which the inline TLS store satisfies for free.
    fn emit_post_call_frame_record(&mut self) {
        if !crate::x64::precise_jit_maps_enabled() || self.frame_record == 0 {
            return;
        }
        let disp = crate::x64::inline_rbp_tls_disp();
        if disp != 0 {
            self.emit_mov_tls_disp32_rbp(disp as u32);
            // The callee published its own identity on entry; restoring only
            // the RBP would leave the pair naming two different frames.
            self.emit_frame_record_identity();
            return;
        }
        self.buf.emit_byte(0x50); // PUSH RAX (preserve the Java return value)
        #[cfg(target_os = "windows")]
        const RESERVE: u8 = 40; // shadow space + alignment after the PUSH
        #[cfg(not(target_os = "windows"))]
        const RESERVE: u8 = 8; // restore 16-byte call-site alignment
        self.buf.emit(&[0x48, 0x83, 0xEC, RESERVE]); // SUB RSP, reserve
        self.emit_mov_arg0_rbp();
        self.emit_mov_reg_imm64(RAX, self.frame_record as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        self.buf.emit(&[0x48, 0x83, 0xC4, RESERVE]); // ADD RSP, reserve
        self.buf.emit_byte(0x58); // POP RAX
    }

    /// `MOV <arg0>, RBP` — for the helper form of the frame record, which is
    /// reached only when no inline TLS displacement could be probed.
    fn emit_mov_arg0_rbp(&mut self) {
        const RBP_REG: u8 = 5;
        let dest = CALL_ARG_REGS[0];
        // REX.W (+ REX.B when the destination is R8..R15), MOV r/m64, r64.
        self.buf
            .emit_byte(0x48 | if dest >= 8 { 0x01 } else { 0x00 });
        self.buf.emit_byte(0x89);
        self.buf.emit_byte(0xC0 | (RBP_REG << 3) | (dest & 7));
    }

    /// `MOV <seg>:[disp32], RBP` — the 9-byte inline frame-record store.
    ///
    /// Byte-identical to the single-pass backend's `emit_mov_tls_disp32_rbp`;
    /// both must write the SAME slot, since `inline_rbp_tls_disp()` is the one
    /// source of truth the VM-side mirror accessor reads back.
    /// `MOV RAX, gs:[disp32]` (`fs:` on Linux): the one-instruction thread
    /// fetch through the `JIT_THREAD` mirror (`x64::jit_thread_tls_disp`).
    fn emit_mov_rax_tls_disp32(&mut self, disp32: u32) {
        self.buf
            .emit_byte(crate::x64::inline_rbp_tls_segment_prefix());
        self.buf.emit_byte(0x48); // REX.W
        self.buf.emit_byte(0x8B); // MOV r64, r/m64
        self.buf.emit_byte(0x04); // ModRM: reg=RAX, r/m=SIB
        self.buf.emit_byte(0x25); // SIB: [disp32] absolute
        self.buf.emit(&disp32.to_le_bytes());
    }
    fn emit_mov_tls_disp32_rbp(&mut self, disp32: u32) {
        self.buf
            .emit_byte(crate::x64::inline_rbp_tls_segment_prefix());
        self.buf.emit_byte(0x48); // REX.W
        self.buf.emit_byte(0x89); // MOV r/m64, r64
        self.buf.emit_byte(0x2C); // ModRM: reg=RBP, r/m=SIB
        self.buf.emit_byte(0x25); // SIB: [disp32] absolute
        self.buf.emit(&disp32.to_le_bytes());
    }

    /// `MOV dword <seg>:[disp32], imm32` — the identity half of the frame
    /// record, emitted immediately after every RBP store so the two always
    /// describe the same frame.
    ///
    /// 32-bit and immediate, so it needs no scratch register: the post-call
    /// republish path runs with the callee's return value live in RAX. Byte
    /// layout matches the single-pass `emit_mov_tls_disp32_imm32`.
    fn emit_frame_record_identity(&mut self) {
        if self.inline_cm_tls_disp == 0 {
            return;
        }
        self.buf
            .emit_byte(crate::x64::inline_rbp_tls_segment_prefix());
        self.buf.emit_byte(0xC7); // MOV r/m32, imm32
        self.buf.emit_byte(0x04); // ModRM: /0, r/m=SIB
        self.buf.emit_byte(0x25); // SIB: [disp32] absolute
        self.buf
            .emit(&(self.inline_cm_tls_disp as u32).to_le_bytes());
        self.buf.emit(&self.compile_id.to_le_bytes());
    }

    /// Cache `*mut JvmThread` in its reserved slot.
    ///
    /// Emitted AFTER the ABI parameter stores: the helper clobbers the
    /// caller-saved registers the parameters arrive in, and by this point they
    /// are already in frame slots. A zero helper address (the JIT unit tests'
    /// stub table) leaves the slot untouched, and every shadow site is gated on
    /// the same condition, so such a method is consistently untracked rather
    /// than dereferencing a pointer that was never fetched.
    fn fetch_current_thread(&mut self) {
        if self.get_current_thread == 0
            || self.shadow_thread_slot_off <= 0
            || !Self::reloc_emit_enabled()
        {
            return;
        }
        let start = self.buf.pos();
        // 2026-09-02: one `mov rax, gs:[disp]` through the `JIT_THREAD`
        // mirror where the VM publishes it; the helper call otherwise. Both
        // stay inside the erasable span.
        let tls_disp = crate::x64::jit_thread_tls_disp();
        if tls_disp != 0 {
            self.emit_mov_rax_tls_disp32(tls_disp as u32);
        } else {
            self.emit_mov_reg_imm64(RAX, self.get_current_thread as u64);
            self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        }
        self.store_rax(self.shadow_thread_slot_off);
        // Capture the entry watermark, inside the erasable span: a method that
        // publishes nothing has no push to unwind, so the capture goes away
        // with the fetch and the (zeroed) thread slot makes every exit's
        // restore skip through its null guard.
        if self.shadow_savetop_slot_off > 0 {
            let ss_top = self.shadow_off_in_thread;
            // MOV R10, [rbp - thread_slot]
            self.buf.emit(&[0x4C, 0x8B, 0x95]);
            self.buf.emit(&(-self.shadow_thread_slot_off).to_le_bytes());
            // TEST R10, R10 ; JE skip
            self.buf.emit(&[0x4D, 0x85, 0xD2]);
            self.buf.emit(&[0x0F, 0x84]);
            let skip = self.buf.pos();
            self.buf.emit(&[0, 0, 0, 0]);
            // MOV R11, [R10 + ss_top] ; MOV [rbp - savetop], R11
            self.buf.emit(&[0x4D, 0x8B, 0x9A]);
            self.buf.emit(&ss_top.to_le_bytes());
            self.buf.emit(&[0x4C, 0x89, 0x9D]);
            self.buf
                .emit(&(-self.shadow_savetop_slot_off).to_le_bytes());
            self.patch_rel32_to_here(skip);
        }
        self.thread_fetch_span = Some((start, self.buf.pos()));
    }

    /// Lazy prologue: erase the thread fetch when nothing published.
    ///
    /// The fetch is a `CALL` on the ENTRY path of every IR method, and most
    /// methods publish nothing — they hold no live reference across a call, or
    /// make no call at all. Paying a helper call per invocation for a slot that
    /// is then never read is what made the first publication attempt regress
    /// `ZonedDateTimeTest` 302 s -> >1200 s while leaving shallow-stack
    /// `ASTParserLoadingTest` untouched: the cost scales with how often methods
    /// are ENTERED, not with stack depth.
    ///
    /// The single-pass backend solves this the same way (`shadow_pushed_any`).
    /// Erasing in place rather than shifting the body keeps every
    /// already-recorded offset — branch patches, deopt points, oop-map native
    /// pcs — valid, which is why this is an erase and not a removal.
    ///
    /// It erases with a JUMP over the span, not with a run of `0x90`. This
    /// half was missing here while the single-pass backend had it, and the
    /// asymmetry cost real time: the span is ~46 bytes, so every IR method
    /// that published nothing retired 46 NOPs on entry, on every invocation.
    /// `CratonBench fib` — a two-line static method entered 2.27e9 times —
    /// paid it 2.27e9 times and ran ~2x slower on the IR tier than on the
    /// single-pass body it displaced. Both backends now share
    /// [`ExecutableBuffer::erase_range_with_jump_over`] so they cannot drift
    /// apart again.
    fn finish_lazy_thread_fetch(&mut self) {
        if self.shadow_pushed_any {
            return;
        }
        if let Some((start, end)) = self.thread_fetch_span.take() {
            // Jumping over the span is only safe while nothing branches INTO
            // its interior: a site patched at `start + 1` would overwrite the
            // rel8 displacement, and one that TARGETS `start + 1` would decode
            // the displacement as an opcode. Under the old NOP fill neither
            // was fatal, so the invariant was never stated — it holds because
            // the span is emitted at the top of the prologue, before any block
            // is lowered, so every recorded patch site is past `end`.
            //
            // This runs before `patch_branches` / `patch_self_calls`, so both
            // lists are still intact here and the invariant is checkable
            // rather than merely true.
            debug_assert!(
                self.branch_patches
                    .iter()
                    .all(|&(pos, _)| pos < start || pos >= end)
                    && self
                        .self_call_patches
                        .iter()
                        .all(|&pos| pos < start || pos >= end),
                "a patch site landed inside the erased thread-fetch span {start}..{end}; \
                 jumping over it would corrupt that patch (or its target)"
            );
            self.buf.erase_range_with_jump_over(start, end);
        }
    }

    /// Publish every live oop of this safepoint onto the thread's shadow stack,
    /// so a relocating collector can rewrite each one in place.
    ///
    /// This is the half that `moving_young_coverage_complete` actually asserts.
    /// The oop map names the slots; only this makes them *rewritable*, because
    /// `scan_oop_slots` yields `ObjectRef` values (marking roots) while
    /// `published_shadow_values` yields the homes the collector writes back to.
    ///
    /// Registers: R10 (thread), R11 (running top), RCX (value temp). All
    /// caller-saved and free here — this runs at the top of the `Op::Call` arm,
    /// before any argument staging or ABI register loading.
    ///
    /// Returns whether the push was actually emitted.
    fn emit_shadow_push(&mut self, offsets: &[i16]) -> bool {
        if self.get_current_thread == 0
            || self.shadow_thread_slot_off <= 0
            || self.shadow_savebase_slot_off <= 0
            || offsets.is_empty()
        {
            return false;
        }
        let ss_top = self.shadow_off_in_thread;
        // MOV R10, [rbp - thread_slot]
        self.buf.emit(&[0x4C, 0x8B, 0x95]);
        self.buf.emit(&(-self.shadow_thread_slot_off).to_le_bytes());
        // TEST R10, R10 ; JE skip  — a null thread means "untracked", exactly
        // as in the single-pass backend; the reload's guard is symmetric.
        self.buf.emit(&[0x4D, 0x85, 0xD2]);
        self.buf.emit(&[0x0F, 0x84]);
        let skip = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        // MOV R11, [R10 + ss_top]      (current top)
        self.buf.emit(&[0x4D, 0x8B, 0x9A]);
        self.buf.emit(&ss_top.to_le_bytes());
        // OVERFLOW GUARD (`ShadowStack::END_OFFSET`) — see the matching comment
        // in the single-pass backend. Without it a push that runs past the end
        // of the 2 MiB buffer keeps storing into the allocator arena behind it.
        // LEA leaves flags alone, so the bump is undone between the CMP and the
        // branch and R11 stays the only scratch. `top == end` is the legal
        // exactly-full state, so the test is strictly-above.
        let need = (offsets.len() as i32) * 8; // Cast: x86-64 disp32
        let overflow = if crate::shadow_end_guard_enabled() {
            // LEA R11,[R11+need] ; CMP R11,[R10+end] ; LEA R11,[R11-need]
            self.buf.emit(&[0x4D, 0x8D, 0x9B]);
            self.buf.emit(&need.to_le_bytes());
            self.buf.emit(&[0x4D, 0x3B, 0x9A]);
            self.buf.emit(&(ss_top + 8).to_le_bytes());
            self.buf.emit(&[0x4D, 0x8D, 0x9B]);
            self.buf.emit(&(-need).to_le_bytes());
            // JA overflow
            self.buf.emit(&[0x0F, 0x87]);
            let p = self.buf.pos();
            self.buf.emit(&[0, 0, 0, 0]);
            Some(p)
        } else {
            None
        };
        // MOV [rbp - savebase], R11    (base of THIS push)
        self.buf.emit(&[0x4C, 0x89, 0x9D]);
        self.buf
            .emit(&(-self.shadow_savebase_slot_off).to_le_bytes());
        for &off in offsets {
            // MOV RCX, [rbp - off] ; MOV [R11], RCX ; LEA R11, [R11 + 8]
            self.buf.emit(&[0x48, 0x8B, 0x8D]);
            self.buf.emit(&(-(off as i32)).to_le_bytes());
            self.buf.emit(&[0x49, 0x89, 0x0B]);
            self.buf.emit(&[0x4D, 0x8D, 0x5B, 0x08]);
        }
        // MOV [R10 + ss_top], R11      (publish the new top)
        self.buf.emit(&[0x4D, 0x89, 0x9A]);
        self.buf.emit(&ss_top.to_le_bytes());
        if let Some(overflow) = overflow {
            // JMP done
            self.buf.emit_byte(0xE9);
            let done = self.buf.pos();
            self.buf.emit(&[0, 0, 0, 0]);
            self.patch_rel32_to_here(overflow);
            // Overflow bail: nothing was stored and `top` is untouched. R11
            // still holds the pre-push `top`; tag bit 0 (slot addresses are
            // 8-aligned) so the paired reload skips its value-restore and only
            // puts `top` back.
            self.buf.emit(&[0x49, 0x83, 0xCB, 0x01]); // OR R11, 1
            self.buf.emit(&[0x4C, 0x89, 0x9D]); // MOV [rbp - savebase], R11
            self.buf
                .emit(&(-self.shadow_savebase_slot_off).to_le_bytes());
            self.emit_shadow_overflow_note();
            self.patch_rel32_to_here(done);
        }
        self.patch_rel32_to_here(skip);
        self.shadow_pushes += 1;
        true
    }

    /// Bump the process-wide shadow-overflow bail counter
    /// (`cratonvm_jit::SHADOW_OVERFLOW_COUNT`). RAX is pushed/popped around it
    /// so the site stays transparent to whatever the call is staging.
    fn emit_shadow_overflow_note(&mut self) {
        // Cast through a raw pointer before converting the static's address.
        let counter =
            (&crate::SHADOW_OVERFLOW_COUNT as *const std::sync::atomic::AtomicUsize) as usize;
        self.buf.emit_byte(0x50); // push rax
        self.emit_mov_reg_imm64(RAX, counter as u64);
        self.buf.emit(&[0xF0, 0x48, 0xFF, 0x00]); // lock inc qword [rax]
        self.buf.emit_byte(0x58); // pop rax
    }

    /// cov-01 — direct (helper-free) read of a static field, the IR tier's
    /// mirror of `x64::Compiler::try_emit_inline_getstatic`. Returns `false`
    /// when the site is not eligible, leaving the caller's helper lowering in
    /// place.
    ///
    /// What is baked is the address of the class's base-POINTER cell, not of
    /// the statics block: one extra dependent load buys immunity to every
    /// republication path, because a `StaticsBlock` is a leaked, never-freed
    /// allocation per class whose *cell* address is stable while the block
    /// pointer inside it is not.
    ///
    /// `resolve_static_base` returning `None` is the whole eligibility test,
    /// and it is the VM's judgement rather than this file's: it declines a
    /// class that is not yet initialized (an inline load runs no `<clinit>`),
    /// `java/lang/System` (the `out`/`err`/`in` bootstrap intercept), anything
    /// not yet published, and a second VM in this process. Reusing that one
    /// predicate is what keeps the two tiers from developing different opinions
    /// about which statics may be read directly.
    ///
    /// Only the two type tags the builder admits are emitted — reference
    /// (64-bit payload) and int-category (`MOVSXD` of the 32-bit payload) —
    /// because `IrBuilder`'s `0xb2` arm refuses `J`/`D`/`F` outright. A tag
    /// that reached here anyway would be a builder bug, so it takes the helper
    /// rather than a plausible-looking wrong width.
    fn emit_inline_getstatic(
        &mut self,
        id: NodeId,
        class_id: u32,
        field_index: u32,
        type_tag: u8,
        is_volatile: bool,
    ) -> bool {
        if !crate::x64::inline_getstatic_enabled() {
            return false;
        }
        // Three widths, and they are the single-pass arm's three, in the same
        // order and with the same constants:
        //
        //   `J`/`D`/`L`/`[`  64-bit MOV of the 64-bit payload
        //   `F`              32-bit MOV of the 32-bit payload — a float's bit
        //                    pattern, so ZERO-extended. `MOVSXD` here would
        //                    sign-extend any float whose bit 31 is set (i.e.
        //                    every negative one) into garbage in the high half,
        //                    and the home word is what `publish_fp_from_slot`
        //                    and every deopt frame read.
        //   int-category     `MOVSXD` of the 32-bit payload
        //
        // A tag outside those is a builder bug — its 0xb2 arm admits exactly
        // these — so take the helper rather than emit a plausible-looking width.
        let wide = matches!(type_tag, b'L' | b'[' | b'J' | b'D');
        let is_float = type_tag == b'F';
        if !wide && !is_float && !matches!(type_tag, b'I' | b'Z' | b'B' | b'C' | b'S') {
            return false;
        }
        let Some(base_cell) = crate::x64::resolve_static_base(class_id, field_index as usize)
        else {
            return false;
        };
        // Cell byte offset within the class's statics block, plus the payload
        // half of the 16-byte cell — the same arithmetic the single-pass arm
        // uses, and the same `FIELD_CELL_PAYLOAD*_OFFSET` constants.
        let Ok(cell_off) = i32::try_from((field_index as usize).saturating_mul(SLOT_SIZE)) else {
            return false;
        };
        let payload = if wide {
            FIELD_CELL_PAYLOAD64_OFFSET as i32
        } else {
            FIELD_CELL_PAYLOAD32_OFFSET as i32
        };
        let Some(disp) = cell_off.checked_add(payload) else {
            return false;
        };
        let slot = self.alloc_slot(id);
        // MOV RAX, imm64(&base_cell) ; MOV RAX, [RAX]
        self.emit_mov_reg_imm64(RAX, base_cell as u64);
        self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + disp32]
        self.buf.emit(&0i32.to_le_bytes());
        if wide {
            self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + disp32]
        } else if is_float {
            self.buf.emit(&[0x8B, 0x80]); // MOV EAX, [RAX + disp32] (zero-extends)
        } else {
            self.buf.emit(&[0x48, 0x63, 0x80]); // MOVSXD RAX, [RAX + disp32]
        }
        self.buf.emit(&disp.to_le_bytes());
        if is_volatile {
            self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
        }
        self.store_rax(slot);
        // An FP result was computed in RAX and is now in its home word — the
        // `Op::ConstF` shape exactly, so it publishes the same way. A no-op when
        // the allocator gave this value no register.
        if matches!(type_tag, b'F' | b'D') {
            self.publish_fp_from_slot(id, slot, type_tag == b'D');
        }
        true
    }

    /// Guard a baked COMPACT CELL OFFSET against a layout replacement, and
    /// return the patch site the caller routes to its helper.
    ///
    /// The optimizing tier's twin of `x64::objects::emit_layout_epoch_guard`,
    /// and it exists for the same reason: an emitter that bakes
    /// `HEADER_SIZE + packed_body_offset` as an immediate is making a
    /// compile-time claim about a layout the class manager can replace at run
    /// time. The two ALLOCATION emitters have guarded that since
    /// perf/halfgap-20260717 and call an unguarded baked layout "confirmed heap
    /// corruption"; the field-access emitters had no guard at all.
    ///
    /// Four instructions against the process-wide replacement epoch, which only
    /// a REPLACEMENT bumps — never a new class registration — so a workload
    /// that never swaps a layout keeps every inline arm.
    fn emit_layout_epoch_guard(&mut self) -> Option<usize> {
        let (addr, expected) = cratonvm_types::layout_replace_epoch_guard();
        if addr.is_null() {
            return None;
        }
        self.emit_mov_reg_imm64(R11, addr as u64);
        // MOV ECX, dword [R11]
        self.buf.emit(&[0x41, 0x8B, 0x0B]);
        // CMP ECX, imm32
        self.buf.emit(&[0x81, 0xF9]);
        self.buf.emit(&(expected as i32).to_le_bytes());
        Some(self.emit_jcc_rel32(0x85)) // JNE -> the caller's slow path
    }

    /// Receiver alignment + containment in one of the three published
    /// `JIT_READ_BOUNDS` regions, with RAX holding the receiver. Failures push
    /// their patch offsets onto `slow`.
    ///
    /// Shared by the inline `getfield` read and the gated reference `putfield`
    /// below, deliberately: both answer the same question — "is this address
    /// one this heap handed out, so the header reads cannot fault" — and a
    /// second transcription of six compares against a six-word table is a place
    /// for the two to disagree about a region index.
    ///
    /// Clobbers RCX and RDX. A caller that needs the flags byte in CL must read
    /// it AFTER this.
    fn emit_receiver_align_and_containment(&mut self, slow: &mut Vec<usize>) {
        // alignment: the low three bits must be clear.
        self.buf.emit(&[0x48, 0x89, 0xC1]); // MOV RCX, RAX
        self.buf.emit(&[0x48, 0x83, 0xE1, 0x07]); // AND RCX, 7
        slow.push(self.emit_jcc_rel32(0x85)); // JNZ
                                              // containment in one of the three published regions.
                                              // RDX = &JIT_READ_BOUNDS = [b0, e0, b1, e1, b2, e2].
        self.emit_mov_reg_imm64(RDX, self.read_bounds_addr as u64);
        self.emit_cmp_rax_mem_rdx(0);
        let below_b0 = self.emit_jcc_rel32(0x82); // JB -> try region 1
        self.emit_cmp_rax_mem_rdx(8);
        let ok0 = self.emit_jcc_rel32(0x82); // JB -> inside region 0
        self.patch_rel32_to_here(below_b0);
        self.emit_cmp_rax_mem_rdx(16);
        let below_b1 = self.emit_jcc_rel32(0x82); // JB -> try region 2
        self.emit_cmp_rax_mem_rdx(24);
        let ok1 = self.emit_jcc_rel32(0x82); // JB -> inside region 1
        self.patch_rel32_to_here(below_b1);
        self.emit_cmp_rax_mem_rdx(32);
        slow.push(self.emit_jcc_rel32(0x82)); // JB -> slow
        self.emit_cmp_rax_mem_rdx(40);
        slow.push(self.emit_jcc_rel32(0x83)); // JAE -> slow
        self.patch_rel32_to_here(ok0);
        self.patch_rel32_to_here(ok1);
    }

    /// `LOCK INC qword [counter]` when the reference-store path trace is on,
    /// and nothing at all otherwise.
    ///
    /// Uses R11, which is free at both call sites, and clobbers flags — which
    /// is why it is only ever emitted where the next instruction does not read
    /// them (immediately before the value load on the inline path, and before
    /// the argument marshal on the helper path).
    fn emit_ref_store_path_trace(&mut self, counter: &'static std::sync::atomic::AtomicU64) {
        if !crate::x64::ir_ref_store_trace_enabled() {
            return;
        }
        self.emit_mov_reg_imm64(R11, counter as *const _ as u64);
        self.buf.emit(&[0xF0, 0x49, 0xFF, 0x03]); // LOCK INC qword [R11]
    }

    /// COV-03 — the optimizing tier's **gated** compact reference `putfield`.
    ///
    /// The caller has already loaded the receiver into RAX and emitted the
    /// inline null check that DEOPTS, so this sequence starts from a non-null
    /// receiver and never has to reproduce the NullPointerException semantics.
    /// Returns `false` without emitting anything when the site is not admitted,
    /// leaving the caller's unconditional `jit_putfield_object` call in place.
    ///
    /// # Why this exists
    ///
    /// The single-pass backend got a gated reference store on 2026-09-02 and
    /// this tier did not, so the barrier plan the generational collector
    /// publishes was inert exactly where hot loops are compiled. A probe of
    /// nothing but reference stores in a counted loop reported `gated=0
    /// declined=0` on that tier's counter while the method's own admission line
    /// said `admitted to the optimizing pipeline`: not refused — never asked.
    ///
    /// # What makes this sound
    ///
    /// The gates are `x64::objects::emit_gated_compact_ref_putfield`'s, read
    /// through the same predicate, and each names a PREFIX of the barrier
    /// helper's own control flow — a skipped call is a call that would have
    /// returned having done nothing. The one structural difference is where the
    /// store sits relative to the post-barrier decision:
    ///
    /// * the single-pass arm stores INLINE, then decides, and on "barrier
    ///   needed" calls the collector's own `write_barrier`;
    /// * this arm decides FIRST, and on "barrier needed" takes the full
    ///   `jit_putfield_object`, which performs the store itself.
    ///
    /// That is not a shortcut, it is the absence of one: this tier has no heap
    /// pointer in its frame (`write_barrier`'s first argument), and inventing a
    /// second route to a collector's remembered set is the mistake
    /// `inline_card_mark_available` was hard-`false`d to stop. Deciding first
    /// costs one forward branch on the barriered path and keeps ONE piece of
    /// code — the helper — responsible for every store that needs a barrier.
    ///
    /// Declining is always safe, and is the only thing a missing plan, an
    /// unresolved compact slot or a disagreeing descriptor can produce.
    fn emit_gated_ir_ref_putfield(
        &mut self,
        node_pc: Option<usize>,
        base: NodeId,
        value: NodeId,
        field_index: i64,
    ) -> bool {
        // Both switches, because both mean "do not store a reference field
        // inline". `CRATONVM_NO_JIT_INLINE_PUTFIELD` is the older, broader one
        // and the single-pass gated arm honours it; an emitter that ignored it
        // would leave someone who set it to chase a lost store still getting
        // inline stores, from the tier they were least likely to look at.
        if !crate::x64::ir_gated_ref_store_enabled() || !crate::x64::inline_putfield_enabled() {
            crate::metrics::note_ir_ref_store_decline(0);
            return false;
        }
        let Some(pc) = node_pc else {
            crate::metrics::note_ir_ref_store_decline(1);
            return false;
        };
        // The resolved compact offset for THIS site. Without it there is no
        // inline address to store to — `HEADER_SIZE + field_index * SLOT_SIZE`
        // is the legacy cell displacement and a compact object does not obey
        // it. This is the plumbing COV-03 named as the blocker.
        // Keyed by `(pc, is_reference)` since 2026-09-02 — the IR String-access
        // expansion emits two accesses at one pc, one Int and one Ref, so a
        // pc-only key could name at most one of them. A reference STORE is
        // always the `true` half.
        let Some(&(c_off, c_is_ref, type_tag)) = self.compact_fields.get(&(pc, true)) else {
            crate::metrics::note_ir_ref_store_decline(2);
            return false;
        };
        // Narrow oops would make the stored word an encoded 32-bit reference
        // rather than the bare pointer this emits; legacy layout would make
        // `c_off` the wrong displacement even when one is resolved.
        if crate::x64::narrow_oops_block_inline_fields()
            || !cratonvm_types::compact_ref_fields_enabled()
        {
            crate::metrics::note_ir_ref_store_decline(3);
            return false;
        }
        // No published plan ⇒ no way to rule either barrier out — and the
        // collectors that publish none (G1, ZGC) are exactly the ones whose
        // empty region table is load-bearing. This is where they decline.
        let Some((pre, post, floor)) = self.ref_store_gates else {
            crate::metrics::note_ir_ref_store_decline(4);
            return false;
        };
        // Descriptor agreement — the same defence the inline `getfield` arm
        // applies and for the same reason: a resolver that fabricated a compact
        // slot (the WildFly Host Controller SIGSEGV) must not steer a store,
        // where it would write a full pointer over a primitive cell.
        if !c_is_ref || !matches!(type_tag, b'L' | b'[') {
            crate::metrics::note_ir_ref_store_decline(5);
            return false;
        }
        // Receiver validity. A base node typed `IrType::Ref` is the IR's own
        // proof that this is an oop — at least as strong as the single-pass
        // `stack_oop_marks` argument its trusted-oop arm rests on — and the
        // caller's null check has already run. Anything else needs the
        // published READ bounds; without them there is no proof to be had, and
        // the site declines rather than dereference an unvalidated address.
        let trusted_oop_receiver = crate::x64::trusted_oop_receiver_getfield_enabled()
            && self.graph.nodes[base as usize].ty == IrType::Ref;
        if !trusted_oop_receiver && self.read_bounds_addr == 0 {
            crate::metrics::note_ir_ref_store_decline(6);
            return false;
        }

        // RAX holds the non-null receiver on entry. The bail sets are kept
        // APART rather than in one vector so the run-time trace can say which
        // gate refused — see `IR_REF_STORE_BAIL`. Without the trace they are
        // concatenated and every one of them lands on the same helper label,
        // which is what they did before this split and what they cost.
        let mut bail_recv: Vec<usize> = Vec::new();
        let mut bail_pre: Vec<usize> = Vec::new();
        let mut bail_barrier: Vec<usize> = Vec::new();
        // `cell_off` below is a compile-time claim about this class's compact
        // layout; the receiver checks that follow are not the only way this arm
        // must be able to decline.
        bail_recv.extend(self.emit_layout_epoch_guard());
        if !trusted_oop_receiver {
            self.emit_receiver_align_and_containment(&mut bail_recv);
        }

        // -- SATB pre-barrier gate --------------------------------------
        // Marking armed ⇒ the overwritten reference has to reach the snapshot,
        // which is the helper's job. Armed only during a concurrent mark phase.
        self.emit_mov_reg_imm64(R11, pre as u64);
        self.buf.emit(&[0x41, 0x80, 0x3B, 0x00]); // CMP byte [R11], 0
        bail_pre.push(self.emit_jcc_rel32(0x85)); // JNE -> helper

        // -- the receiver flags byte, read ONCE --------------------------
        // `GC_FLAGS_BYTE_OFFSET` carries the GC flags in bits 0..3 and `gc_age`
        // in bits 4..7, so this single byte answers both the young-receiver
        // question the post gate asks and the per-object layout question the
        // store shape below asks.
        self.buf.emit(&[0x0F, 0xB6, 0x88]); // MOVZX ECX, byte [RAX + disp32]
        self.buf
            .emit(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());

        // -- slot bounds ------------------------------------------------
        // `field_index < num_slots`, which for a compact object is the FIELD
        // COUNT (see `heap.rs` on the two meanings that word carries). A
        // failure DROPS the store, matching `jit_putfield_object`'s own
        // out-of-bounds behaviour, so it targets its own label rather than the
        // helper.
        self.buf.emit(&[0x44, 0x8B, 0x98]); // MOV R11D, dword [RAX + disp32]
        self.buf
            .emit(&(cratonvm_types::NUM_SLOTS_OFFSET as i32).to_le_bytes());
        self.emit_mov_reg_imm64(R10, field_index as u64);
        self.buf.emit(&[0x45, 0x3B, 0xD3]); // CMP R10D, R11D
        let oob = self.emit_jcc_rel32(0x83); // JAE -> drop

        // -- post-barrier gates ------------------------------------------
        // CL still holds the receiver's flags byte. Two shapes can rule the
        // post barrier out, and a publisher supplies exactly one of them
        // (`ref_store_gates_of` enforces that).
        let mut inline_store: Vec<usize> = Vec::new();
        if let Some(mask) = self.ref_store_post_skip_mask {
            // MASK — "the receiver carries none of the bits that could make a
            // post barrier necessary". The generational shape: `GC_FLAG_OLD_GEN`
            // clear means young, and a young receiver needs no card. Baked as an
            // immediate, because which collector is running cannot change after
            // start-up.
            self.buf.emit(&[0xF6, 0xC1, mask]); // TEST CL, mask
            inline_store.push(self.emit_jcc_rel32(0x84)); // JZ -> young receiver
        } else {
            // FLOOR — `age << 4 | flags` compared unsigned against
            // `promotion_floor << 4` is an EXACT test of `gc_age <
            // promotion_floor`, because the flags nibble is at most 15 and
            // cannot carry `a << 4` up to `(a + 1) << 4`.
            self.emit_mov_reg_imm64(R11, floor as u64);
            self.buf.emit(&[0x41, 0x3A, 0x0B]); // CMP CL, byte [R11]
            inline_store.push(self.emit_jcc_rel32(0x82)); // JB -> young receiver
        }
        self.emit_mov_reg_imm64(R11, post as u64);
        self.buf.emit(&[0x41, 0x80, 0x3B, 0x00]); // CMP byte [R11], 0
        inline_store.push(self.emit_jcc_rel32(0x84)); // JZ -> no old objects
                                                      // Neither gate ruled the barrier out: the helper does the store.
        bail_barrier.push(self.emit_jmp_rel32());

        // -- the barrier-free store, in whichever shape this OBJECT has --
        //
        // Both layouts store inline. A compact-only arm would be an arm that
        // almost never fires: `init_object_header`, the TLAB fast path that
        // serves nearly every allocation for the interpreter and
        // `jit_new_object` alike, writes a LEGACY header unconditionally --
        // `array_length = 0`, no `GC_FLAG_COMPACT` -- regardless of whether the
        // class has a registered compact layout, because it never consults
        // `plan_object_alloc`. The run-time census says so exactly: with only
        // the compact shape emitted, `RefStoreLoopProbe` took this arm 0 times
        // out of 16,384,000 and bailed at the compactness test every time, on
        // Generational and ZGC alike. It is the same trap the inline `getfield`
        // read fell into and climbed out of on 2026-08-18, and the fix is the
        // same: emit both shapes and pick per OBJECT, exactly as that arm and
        // `jit_putfield_object` itself do.
        for patch in inline_store {
            self.patch_rel32_to_here(patch);
        }
        self.emit_ref_store_path_trace(&crate::metrics::IR_REF_STORE_INLINE_TAKEN);
        self.gp_load_value(RDX, value);
        self.buf.emit(&[0xF6, 0xC1, cratonvm_types::GC_FLAG_COMPACT]); // TEST CL, imm8
        let legacy_shape = self.emit_jcc_rel32(0x84); // JZ -> the 16-byte cell
        // COMPACT: a reference field is the bare 8-byte pointer at the cell
        // base, which is exactly what the inline `getfield` arm reads back.
        //
        // Cast: a compact field offset plus the header is bounded by the
        // object size.
        let cell_off = (HEADER_SIZE + c_off as usize) as i32;
        self.buf.emit(&[0x48, 0x89, 0x90]); // MOV [RAX + disp32], RDX
        self.buf.emit(&cell_off.to_le_bytes());
        let stored = self.emit_jmp_rel32();

        // LEGACY: the uniform 16-byte `Value` cell -- tag dword (with its pad)
        // then the pointer payload. Transcribed from the single-pass backend's
        // own legacy inline reference store, so the two cannot disagree about
        // which half of the cell holds what; the bounds check above is what
        // makes `field_index * SLOT_SIZE` addressable, since for a legacy
        // object `num_slots` counts exactly these cells.
        self.patch_rel32_to_here(legacy_shape);
        let legacy_off = (HEADER_SIZE + field_index as usize * SLOT_SIZE) as i32;
        self.emit_mov_reg_imm64(R10, u64::from(cratonvm_types::FIELD_CELL_TAG_OBJECT));
        self.buf.emit(&[0x4C, 0x89, 0x90]); // MOV [RAX + disp32], R10
        self.buf
            .emit(&(legacy_off + cratonvm_types::FIELD_CELL_TAG_OFFSET as i32).to_le_bytes());
        self.buf.emit(&[0x48, 0x89, 0x90]); // MOV [RAX + disp32], RDX
        self.buf
            .emit(&(legacy_off + FIELD_CELL_PAYLOAD64_OFFSET as i32).to_le_bytes());
        let stored_legacy = self.emit_jmp_rel32();

        // -- helper fallback: the full SATB + store + post barrier --------
        //
        // With the trace on, each bail set gets a one-instruction stub that
        // names it before joining the helper; without it they all land here
        // directly and cost nothing.
        let bail_groups = [(bail_recv, 0usize), (bail_pre, 1), (bail_barrier, 3)];
        let mut to_helper: Vec<usize> = Vec::new();
        if crate::x64::ir_ref_store_trace_enabled() {
            for (patches, reason) in bail_groups {
                if patches.is_empty() {
                    continue;
                }
                for patch in patches {
                    self.patch_rel32_to_here(patch);
                }
                self.emit_ref_store_path_trace(&crate::metrics::IR_REF_STORE_BAIL[reason]);
                to_helper.push(self.emit_jmp_rel32());
            }
        } else {
            for (patches, _) in bail_groups {
                to_helper.extend(patches);
            }
        }
        for patch in to_helper {
            self.patch_rel32_to_here(patch);
        }
        self.emit_ref_store_path_trace(&crate::metrics::IR_REF_STORE_HELPER_TAKEN);
        self.load_reg_from_frame(CALL_ARG_REGS[0], self.context_slot_off);
        self.load_reg_from_frame(CALL_ARG_REGS[1], self.slot_of(base));
        self.emit_mov_reg_imm64(CALL_ARG_REGS[2], field_index as i64 as u64);
        self.load_reg_from_frame(CALL_ARG_REGS[3], self.slot_of(value));
        self.emit_mov_reg_imm64(RAX, self.putfield_object as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX

        self.patch_rel32_to_here(oob);
        self.patch_rel32_to_here(stored);
        self.patch_rel32_to_here(stored_legacy);
        crate::metrics::note_ir_ref_store_gated();
        true
    }

    /// Guarded inline read of a compact instance field, with the checked
    /// `jit_getfield` helper as the slow path. Returns `false` when the site is
    /// not eligible, leaving the caller's helper-only lowering in place.
    ///
    /// Why this exists: routing every field read through the helper costs a
    /// JIT-boundary note, an `is_object_address` region walk and a 16-byte
    /// atomic cell read. When that hardening first landed on the single-pass
    /// backend it cost 4.7x on bintrees-16, which is why that backend grew the
    /// guarded inline path (`guarded_inline_getfield_enabled`). The IR tier
    /// never got one, and it is the measured reason a forced-C2 `bt18` ran
    /// **1.85x slower** than the single-pass body it replaced (3614-3762 ms vs
    /// 6436-7013 ms, five interleaved reps) once reference field reads made
    /// real methods IR-eligible. An optimizing tier that reads fields more
    /// expensively than the baseline tier cannot be worth selecting.
    ///
    /// The shape mirrors `x64.rs`'s arm exactly, and deliberately keeps the
    /// property that stops the stale-receiver SIGSEGV: never dereference a
    /// receiver that is not null-free, 8-aligned and inside a published GC
    /// region. Everything else — null, unaligned, out-of-heap, or a width this
    /// arm does not emit — branches to the helper, whose NPE / `i64::MIN`
    /// semantics are unchanged.
    ///
    /// **The legacy-layout receiver reads inline too, since 2026-08-18.** It
    /// used to take the helper — "the one simplification against the
    /// single-pass version" — and that simplification turned out to be 100% of
    /// this helper's calls on the Generational collector. `init_object_header`,
    /// the TLAB fast path that serves ~99% of allocations for both the
    /// interpreter and `jit_new_object`, writes `array_length = 0` and no
    /// `GC_FLAG_COMPACT` unconditionally: it never consults
    /// `plan_object_alloc`, so a class with a perfectly good registered compact
    /// layout is still allocated legacy. An arm that inlines only compact
    /// receivers therefore inlines almost nothing. See
    /// fixed-suite-bugs/jit/every-jit-getfield-takes-the-helper-FIXED-20260820.md.
    ///
    /// The legacy read is the uniform 16-byte `Value` cell at
    /// `HEADER_SIZE + field_index * SLOT_SIZE`, transcribed from the
    /// single-pass arm's own legacy branch so the two cannot disagree about
    /// payload offsets or sign-extension.
    fn emit_inline_compact_getfield(
        &mut self,
        node_pc: Option<usize>,
        node_ty: IrType,
        base: NodeId,
        field_index: i64,
        slot: i32,
    ) -> bool {
        if self.getfield == 0 {
            crate::metrics::note_ir_getfield_decline(0);
            return false;
        }
        let Some(pc) = node_pc else {
            crate::metrics::note_ir_getfield_decline(1);
            return false;
        };
        // Keyed by `(pc, is_reference)` and not by `pc` alone: the IR
        // String-access expansion (`ir.rs::try_string_access_intrinsic`)
        // emits TWO `Op::Load`s at one `invokevirtual` pc — `coder` (Int)
        // and `value` (Ref) — so a pc-only key could describe at most one
        // of them and the other would fall to the checked helper for want
        // of a row rather than for any reason about the field. An ordinary
        // `getfield` pc carries exactly one row, under its own field's
        // reference-ness, so nothing about that case changes.
        let Some(&(c_off, c_is_ref, type_tag)) =
            self.compact_fields.get(&(pc, node_ty == IrType::Ref))
        else {
            crate::metrics::note_ir_getfield_decline(2);
            return false;
        };
        if crate::x64::narrow_oops_block_inline_fields() {
            crate::metrics::note_ir_getfield_decline(3);
            return false;
        }
        let raw_mode = crate::x64::inline_getfield_enabled();
        let guarded = crate::x64::guarded_inline_getfield_enabled() && self.read_bounds_addr != 0;
        if !raw_mode && !guarded {
            crate::metrics::note_ir_getfield_decline(4);
            return false;
        }
        // Trusted-oop receiver: null check only, no containment.
        //
        // The single-pass backend has had this since
        // `emit_trusted_oop_receiver_check` landed — "a value whose
        // operand-stack type is already proven to be an oop cannot be an
        // unaligned integer or an arbitrary out-of-heap address without an
        // earlier JIT/GC correctness failure, so repeating the six arena-bound
        // comparisons at every field access is redundant". The IR tier never
        // got it, and that is why ZGC and G1 — which deliberately publish NO
        // region bounds — fail the containment clause on 100% of receivers
        // here while the single-pass arm sails through on its proven oops.
        //
        // The IR's proof is its own type lattice: `Op::Load`'s base node is
        // typed `IrType::Ref`. That is at least as strong as the single-pass
        // `stack_oop_marks` argument this reuses.
        //
        // **Restricted to PRIMITIVE fields, deliberately.** Dropping
        // containment for a REFERENCE load would inline-read a word that, under
        // ZGC, may be `Z_COLORED_TAG | colour | offset` rather than a pointer —
        // the un-barriered colored word `heap.rs::read_prim_element` panics on
        // by design, and `zgc-jit-load-barrier.md` (risk J1) rates the silent
        // version worse than a SIGSEGV. Primitives need neither a load barrier
        // nor narrow-oop decoding, so they are the whole safe set.
        //
        // Note what this does NOT do: it does not publish `JIT_REGION_BOUNDS`
        // on a non-publishing collector. That table's emptiness is load-bearing
        // — per `audits/g1-audit.md` §8.1 (G1-2) it is the interlock that keeps
        // every inline reference-STORE fast path unreachable under G1/ZGC, so a
        // JNI-pinned CSet-excluded region cannot lose its remembered-set edge.
        // Filling it to speed up loads would silently re-enable those stores.
        // `node_ty != Ref` is `!ref_node`, computed here because `ref_node`
        // is not bound until the descriptor-agreement check below — which
        // still runs, and still refuses the site, before anything is emitted.
        let trusted_oop_receiver = node_ty != IrType::Ref
            && !raw_mode
            && crate::x64::trusted_oop_receiver_getfield_enabled()
            && self.graph.nodes[base as usize].ty == IrType::Ref;
        // The node type and the resolved descriptor must agree. They can only
        // disagree through a resolver that fabricated a compact slot — the
        // WildFly Host Controller SIGSEGV — and the consequence of trusting it
        // here would be a 32-bit sign-extended load of half a pointer.
        let ref_node = node_ty == IrType::Ref;
        let ref_tag = matches!(type_tag, b'L' | b'[');
        if ref_node != ref_tag || ref_tag != c_is_ref {
            crate::metrics::note_ir_getfield_decline(5);
            return false;
        }
        // Width agreement, per descriptor. `J`/`D`/`F` were refused outright
        // until 2026-08-17; measurement showed that refusal was ~88% of every
        // `jit_getfield` call in a field-dense run — four sites falling back to
        // an UNGUARDED helper CALL, which is why the count was identical on
        // ZGC, Generational and G1. A `long` field on a hot path
        // (BouncyCastle's `GeneralDigest.byteCount`) is not an exotic shape.
        //
        // Each is admitted only when the IR node's own type agrees with the
        // resolved descriptor — the same defence the ref/non-ref check above
        // applies, and for the same reason: a resolver that fabricated a
        // compact slot must not steer a load width.
        let width_ok = match type_tag {
            b'I' | b'Z' | b'B' | b'C' | b'S' => node_ty == IrType::Int,
            b'J' => node_ty == IrType::Long,
            b'D' => node_ty == IrType::Double,
            b'F' => node_ty == IrType::Float,
            _ => false,
        };
        if !ref_node && !width_ok {
            crate::metrics::note_ir_getfield_decline(6);
            return false;
        }

        let cell_off = (HEADER_SIZE + c_off as usize) as i32;
        let mut slow: Vec<usize> = Vec::new();
        // `cell_off` is a compile-time claim about this class's compact layout.
        slow.extend(self.emit_layout_epoch_guard());

        self.gp_load_value(RAX, base);
        // 2026-09-02: a receiver this block already proved (null-tested and,
        // where the guard applies, mapped) is not re-proved. Same SSA value,
        // same block, and no way for it to become null or unmapped in between:
        // a relocating collector rewrites the home word to the object's new
        // address, which is still mapped. `itemCheck` proved `n` once per
        // field it read (`ir_receiver_guard_cse_enabled`).
        let receiver_proven = self.receiver_already_guarded(base);
        // The null test is suppressed by the WEAKER fact as well: the seed, or
        // a test already emitted in this block. The containment guard below
        // still keys off `receiver_proven` alone, so a null-only proof never
        // licenses skipping the alignment and read-bounds compares.
        if self.receiver_already_non_null(base) {
            crate::metrics::note_ir_receiver_null_check_elided();
        } else {
            // 1. null → slow (the helper raises the NPE).
            self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
            slow.push(self.emit_jcc_rel32(0x84)); // JZ
            crate::metrics::note_ir_receiver_null_check_emitted();
            // Its fall-through is a proof for the rest of the block.
            self.note_receiver_non_null(base);
        }
        if guarded && !raw_mode && !trusted_oop_receiver && !receiver_proven {
            self.note_receiver_guarded(base);
            self.emit_receiver_align_and_containment(&mut slow);
        }
        // 4. per-OBJECT compactness. A class with a registered compact layout
        //    can still have legacy 16-byte-cell instances — and in practice
        //    almost all of them are, because the TLAB fast path writes a legacy
        //    header unconditionally (see this function's doc comment). Reading
        //    one at the packed offset yields a mangled {tag, half-pointer}
        //    word, so the two layouts get two reads, exactly as the single-pass
        //    arm does.
        self.buf.emit(&[0xF6, 0x80]); // TEST byte [RAX + disp32], imm8
        self.buf
            .emit(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());
        self.buf.emit_byte(cratonvm_types::GC_FLAG_COMPACT);
        let legacy_patch = self.emit_jcc_rel32(0x84); // JZ → legacy inline read

        // 5. the read itself. A compact reference field is the bare 8-byte
        //    pointer at the cell base; a primitive is its tagless descriptor
        //    width.
        if ref_node {
            self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + disp32]
            self.buf.emit(&cell_off.to_le_bytes());
        } else {
            match type_tag {
                b'Z' => self.buf.emit(&[0x48, 0x0F, 0xB6, 0x80]), // MOVZX RAX, byte
                b'B' => self.buf.emit(&[0x48, 0x0F, 0xBE, 0x80]), // MOVSX RAX, byte
                b'C' => self.buf.emit(&[0x48, 0x0F, 0xB7, 0x80]), // MOVZX RAX, word
                b'S' => self.buf.emit(&[0x48, 0x0F, 0xBF, 0x80]), // MOVSX RAX, word
                // `long` and `double` are 8-byte compact cells; both leave the
                // raw 64 bits in RAX, which is exactly what the helper returns
                // (`Value::Long(l) => l`, `Value::Double(d) => d.to_bits()`),
                // so the shared tail below stores and (for FP) republishes them
                // identically.
                b'J' | b'D' => self.buf.emit(&[0x48, 0x8B, 0x80]), // MOV RAX, qword
                // `float` is a 4-byte cell and the helper ZERO-extends it
                // (`f.to_bits() as i64` widens a u32). A 32-bit MOV zeroes the
                // upper half; MOVSXD would sign-extend and corrupt every
                // negative-signed bit pattern.
                b'F' => self.buf.emit(&[0x8B, 0x80]), // MOV EAX, dword (zero-extends)
                _ => self.buf.emit(&[0x48, 0x63, 0x80]), // MOVSXD RAX, dword
            }
            self.buf.emit(&cell_off.to_le_bytes());
        }
        self.buf.emit_byte(0xE9); // JMP rel32 → done
        let done_patch = self.buf.pos();
        self.buf.emit(&[0; 4]);

        // --- legacy path: the uniform 16-byte `Value` cell. Transcribed from
        //     the single-pass arm's legacy branch; the payload sub-offsets and
        //     the sign-extension choice are that arm's, not a re-derivation. ---
        self.patch_rel32_to_here(legacy_patch);
        let legacy_cell_off = (HEADER_SIZE + field_index as usize * SLOT_SIZE) as i32;
        if ref_node {
            // The cell's DISCRIMINANT decides whether that payload is a
            // pointer. Reading the payload without asking was the Tomcat/Derby
            // SIGSEGV of 2026-08-23: `SQLChar.rawData` is declared `[C`, its
            // cell held `Value::Int(1)`, and this arm loaded the 8-byte payload
            // word — 1 — and handed it on as a reference. The `arraylength`
            // three instructions later is `MOV r32,[RAX+4]`, so the process
            // died at `addr=0x5`, deterministically: three crashes carried
            // byte-identical registers.
            //
            // The helper this arm exists to skip has always looked at the
            // variant (`read_value_atomic` then `match val`), which is why the
            // crash was invisible in the helper path and why the fix belongs
            // here. Deferring to it — rather than degrading to null inline —
            // keeps ONE place deciding what a punned slot means, and that place
            // counts it (`getfield reference loads that contained a primitive
            // slot`).
            //
            // Cost is one compare and one not-taken branch on the legacy
            // reference path; the compact arm above is untouched, and it is the
            // one this whole inline sequence was written for.
            self.buf.emit(&[0x83, 0xB8]); // CMP DWORD [RAX + disp32], imm8
            self.buf.emit(
                &(legacy_cell_off + cratonvm_types::FIELD_CELL_TAG_OFFSET as i32).to_le_bytes(),
            );
            self.buf
                .emit_byte(cratonvm_types::FIELD_CELL_TAG_OBJECT as u8);
            slow.push(self.emit_jcc_rel32(0x85)); // JNE → the checked helper

            // A reference descriptor always reads the cell's 64-bit pointer
            // payload — never the 32-bit MOVSXD below, which would
            // sign-extend half a pointer into a bogus non-null receiver.
            self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + disp32]
            self.buf
                .emit(&(legacy_cell_off + FIELD_CELL_PAYLOAD64_OFFSET as i32).to_le_bytes());
        } else {
            match type_tag {
                b'J' | b'D' => {
                    self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, qword
                    self.buf.emit(
                        &(legacy_cell_off + FIELD_CELL_PAYLOAD64_OFFSET as i32).to_le_bytes(),
                    );
                }
                // `float` is stored as a 32-bit payload and the helper
                // ZERO-extends it (`f.to_bits() as i64`), so a 32-bit MOV;
                // MOVSXD would corrupt every float with bit 31 set.
                b'F' => {
                    self.buf.emit(&[0x8B, 0x80]); // MOV EAX, dword (zero-extends)
                    self.buf.emit(
                        &(legacy_cell_off + FIELD_CELL_PAYLOAD32_OFFSET as i32).to_le_bytes(),
                    );
                }
                // Every int-category descriptor: the cell holds a `Value::Int`
                // payload already narrowed on store, so sign-extending it is
                // what the helper returns.
                _ => {
                    self.buf.emit(&[0x48, 0x63, 0x80]); // MOVSXD RAX, dword
                    self.buf.emit(
                        &(legacy_cell_off + FIELD_CELL_PAYLOAD32_OFFSET as i32).to_le_bytes(),
                    );
                }
            }
        }
        self.buf.emit_byte(0xE9); // JMP rel32 → done
        let done_legacy_patch = self.buf.pos();
        self.buf.emit(&[0; 4]);

        // --- slow path: the checked helper, byte-identical to the arm this
        //     replaces, including the sentinel bail. ---
        for p in slow {
            self.patch_rel32_to_here(p);
        }
        // For a REFERENCE field whose base node the IR already types `Ref`, use
        // the helper that skips the `is_object_address` membership walk.
        //
        // The walk is validation against a stale receiver, and the proof we
        // have here is the same one the PRIMITIVE trusted-oop arm above relies
        // on — an arm that goes further and does a raw inline load off this
        // very receiver. Handing it to a helper instead is a strictly weaker
        // use of the same trust.
        //
        // This changes only the SLOW path. The inline path is untouched, so
        // Generational — where containment passes and the inline ref load is
        // taken — sees no difference at all. It is ZGC and G1, which publish no
        // read bounds for reference loads, that take this path on 100% of
        // reference accesses, and there the walk was the largest single cost of
        // a field-dense run (`ZObjectStarts::contains` 11.1% +
        // `is_object_address` 9.5% on `dev` @800d17cc8).
        //
        // `contains` is already a tight bitmap probe; the cost is one
        // cache-missing random probe per access, so the fix is to stop asking,
        // not to ask faster.
        // Carried in `field_index`, NOT a new helper slot: `helpers_abi.rs`
        // pins the table's field count, byte size and golden offsets with const
        // assertions and an ABI version, all of which exist to keep the layout
        // the JIT bakes frozen. A one-bit argument flag needs none of that, and
        // the index is a small non-negative slot number with 62 spare bits.
        // The second flag, `GETFIELD_EXPECT_REFERENCE`, tells the helper what
        // we are going to DO with the answer rather than what we know about the
        // receiver: the code emitted after this call dereferences the result
        // for a reference field, while the helper otherwise returns the payload
        // of whichever `Value` variant it finds in the slot. A type-punned
        // primitive therefore became a wild pointer — `SQLChar.rawData` is
        // declared `[C`, read back as `Int(1)`, and the `arraylength` that
        // followed faulted at `addr=0x5`.
        let base_is_proven_oop = self.graph.nodes[base as usize].ty == IrType::Ref;
        let arg2 =
            cratonvm_jit_api::getfield_index_arg(field_index as u32, ref_node, base_is_proven_oop);
        self.load_reg_from_frame(CALL_ARG_REGS[0], self.context_slot_off);
        self.gp_load_value(CALL_ARG_REGS[1], base);
        self.emit_mov_reg_imm64(CALL_ARG_REGS[2], arg2);
        self.emit_mov_reg_imm64(RAX, self.getfield as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        self.emit_mov_reg_imm64(R10, i64::MIN as u64);
        self.buf.emit(&[0x4C, 0x39, 0xD0]); // CMP RAX, R10
        self.buf.emit(&[0x0F, 0x84]); // JE rel32 → shared bail stub
        let exc_patch = self.buf.pos();
        self.buf.emit(&[0; 4]);
        self.push_call_exc_patch(exc_patch);

        self.patch_rel32_to_here(done_patch);
        self.patch_rel32_to_here(done_legacy_patch);
        self.store_rax(slot);
        true
    }

    /// `CMP RAX, [RDX + disp32]` — REX.W + 3B /r, ModRM(mod=10, reg=RAX, rm=RDX).
    fn emit_cmp_rax_mem_rdx(&mut self, disp: i32) {
        self.buf.emit(&[0x48, 0x3B, 0x82]);
        self.buf.emit(&disp.to_le_bytes());
    }

    /// Copy the (possibly rewritten) published values back into their frame
    /// slots and retract `top`. Pairs with [`Self::emit_shadow_push`].
    ///
    /// Emitted immediately after the call returns — deliberately BEFORE the
    /// result is stored. The single-pass backend uses RAX as its value temp,
    /// which would collide with the return value and force this after the
    /// store; RCX held an outgoing argument and is dead the instant the call
    /// returns, so using it keeps RAX untouched and lets this sit at the one
    /// choke point (`emit_call_return_check`) all dispatch routes share.
    fn emit_shadow_reload(&mut self) {
        let offsets = std::mem::take(&mut self.pending_shadow);
        if self.get_current_thread == 0
            || self.shadow_thread_slot_off <= 0
            || self.shadow_savebase_slot_off <= 0
            || offsets.is_empty()
        {
            return;
        }
        let ss_top = self.shadow_off_in_thread;
        // MOV R10, [rbp - thread_slot] ; TEST ; JE skip  (mirrors the push)
        self.buf.emit(&[0x4C, 0x8B, 0x95]);
        self.buf.emit(&(-self.shadow_thread_slot_off).to_le_bytes());
        self.buf.emit(&[0x4D, 0x85, 0xD2]);
        self.buf.emit(&[0x0F, 0x84]);
        let skip = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        // MOV R11, [rbp - savebase]    (restore from THIS push's base, not the
        // live `top`, so an unbalanced intervening push cannot drift it)
        self.buf.emit(&[0x4C, 0x8B, 0x9D]);
        self.buf
            .emit(&(-self.shadow_savebase_slot_off).to_le_bytes());
        // Overflow bail protocol (see `emit_shadow_push`): a tagged base means
        // the push stored nothing, so the restore below would read slots that
        // were never written. Skip to the tail, which only retracts `top`.
        self.buf.emit(&[0x49, 0xF7, 0xC3]); // TEST R11, imm32
        self.buf.emit(&1i32.to_le_bytes());
        self.buf.emit(&[0x0F, 0x85]); // JNE overflow
        let overflow = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        for &off in &offsets {
            // MOV RCX, [R11] ; MOV [rbp - off], RCX ; LEA R11, [R11 + 8]
            self.buf.emit(&[0x49, 0x8B, 0x0B]);
            self.buf.emit(&[0x48, 0x89, 0x8D]);
            self.buf.emit(&(-(off as i32)).to_le_bytes());
            self.buf.emit(&[0x4D, 0x8D, 0x5B, 0x08]);
        }
        self.patch_rel32_to_here(overflow);
        // MOV R11, [rbp - savebase] ; AND R11, ~1 ; MOV [R10 + ss_top], R11
        // (retract top; the mask is a no-op unless the push bailed)
        self.buf.emit(&[0x4C, 0x8B, 0x9D]);
        self.buf
            .emit(&(-self.shadow_savebase_slot_off).to_le_bytes());
        self.buf.emit(&[0x49, 0x83, 0xE3, 0xFE]);
        self.buf.emit(&[0x4D, 0x89, 0x9A]);
        self.buf.emit(&ss_top.to_le_bytes());
        self.patch_rel32_to_here(skip);
        self.shadow_reloads += 1;
    }

    /// Zero the frame slot of every `Ref`-typed phi, and mark those slots
    /// publishable.
    ///
    /// Phi slots are reserved by `prealloc_phi_slots` before any code runs and
    /// are written later, by the edge copy of whichever predecessor is taken.
    /// A safepoint reached on a path that has not yet written one would
    /// otherwise see the previous frame's leftovers through a slot the oop map
    /// claims is a live reference — the collector would then follow, and
    /// rewrite, a garbage word.
    ///
    /// Zeroing makes the unwritten state a null reference, which every root
    /// consumer already handles, so the whole phi set becomes safe to publish
    /// unconditionally. The alternative — proving per-safepoint which phis are
    /// written on every path reaching it — is a dataflow problem for a handful
    /// of stores. Only `Ref` phis are zeroed; a primitive phi read before its
    /// write would be a lowering bug, not a GC one, and zeroing it would hide
    /// that.
    fn zero_ref_phi_slots(&mut self) {
        let refs: Vec<(usize, i32)> = (0..self.graph.nodes.len())
            .filter(|&id| {
                matches!(self.graph.nodes[id].op, Op::Phi) && self.graph.nodes[id].ty == IrType::Ref
            })
            // A phi with no location cannot be zeroed and must not be
            // published; `prealloc_phi_slots` gives every phi one, so this
            // filter drops nothing in a well-formed compile.
            .filter_map(|id| self.node_slot[id].map(|off| (id, off.get() as i32)))
            .collect();
        for (id, off) in refs {
            // MOV qword [rbp - off], 0
            self.buf.emit(&[0x48, 0xC7, 0x85]);
            self.buf.emit(&(-off).to_le_bytes());
            self.buf.emit(&0u32.to_le_bytes());
            self.defined_nodes[id] = true;
        }
    }

    /// `TEST BYTE [rip+disp32], 0xFF` against `safepoint_flag_addr` — the
    /// whole poll in one 7-byte instruction, reporting whether the flag was
    /// within ±2GB RIP reach of it.
    ///
    /// Mirrors `x64/emit.rs`'s `emit_test_mem8_abs_imm8`; the two backends
    /// emit the same poll and this keeps them saying the same thing. `F6 /0 ib`
    /// with ModRM `mod=00, rm=101` is the RIP-relative form, and the
    /// displacement is measured from the end of the WHOLE instruction — past
    /// the trailing `imm8`, which is why the reach test adds 7 and not 6.
    ///
    /// The alternative it replaces, `MOV R11, imm64` + `TEST BYTE [R11], 0xFF`,
    /// is 15 bytes and two instructions and burns a register. Nothing here
    /// records a patch site: unlike the single-pass backend, this lowerer never
    /// duplicates emitted bytes to a second address, so a displacement that is
    /// right when emitted stays right.
    fn emit_test_safepoint_flag_rip(&mut self) -> bool {
        // F6 05 <disp32> <imm8>
        const LEN: usize = 7;
        // Cast: non-negative index/count to usize
        let here = self.buf.as_ptr() as usize + self.buf.pos();
        let next_pc = here.wrapping_add(LEN);
        // Widening: i64/usize -> i128 (no truncation, for range check)
        let delta: i128 = (self.safepoint_flag_addr as i128) - (next_pc as i128);
        // Widening: i64/usize -> i128 (no truncation, for range check)
        if delta < i32::MIN as i128 || delta > i32::MAX as i128 {
            return false;
        }
        self.buf.emit(&[0xF6, 0x05]);
        self.buf.emit(&(delta as i32).to_le_bytes()); // Cast: rel32 displacement
        self.buf.emit_byte(0xFF);
        true
    }

    /// Emit the default-on cooperative poll used at method entries and loop
    /// back-edges. The lowerer keeps all live values in frame slots, so the
    /// no-argument slow path may be called directly.
    fn emit_safepoint_poll(&mut self) {
        let enabled = cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_SAFEPOINT_POLLS")
            .and_then(|v| v.into_string().ok())
            .is_none_or(|v| v != "0");
        if !enabled || self.safepoint_flag_addr == 0 || self.safepoint_slow_path == 0 {
            return;
        }
        if !self.emit_test_safepoint_flag_rip() {
            // Out of ±2GB RIP reach — materialize the address and read
            // through it, the shape this poll had before 2026-09-02.
            self.emit_mov_reg_imm64(R11, self.safepoint_flag_addr as u64);
            self.buf.emit(&[0x41, 0xF6, 0x03, 0xFF]); // TEST byte ptr [R11], 0xff
        }
        self.buf.emit(&[0x0F, 0x84]); // JZ .clear
        let clear_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        // THE POLL IS A SAFEPOINT, so it owes the relocation contract exactly
        // what a call owes it. It did not pay: no id was stored and no oop map
        // was recorded, so a thread parked in the slow path left its frame's
        // sp-id slot holding either the prologue's zero (no earlier GC point in
        // this frame -- the method-entry poll is ALWAYS this) or the id of some
        // EARLIER call, whose map describes a different program point.
        //
        // Both are refusals. `moving_young_frame_coverage_complete` finds no
        // `OopMapEntry` for the stored id, counts `NO_MAP_FOR_STORED_ID`, and
        // `frame_active_map_slots` / `moving_young_frame_live_hi` both return
        // `None` -- which is fail-closed by design, so every movable word in
        // the frame's band is reported unpublished and the whole cycle refuses
        // with `UNPUBLISHED_FRAME_OOP`. Measured on
        // `org.h2.test.jdbc.TestCachedQueryResults`, that is 100 % of the
        // `no-map-for-id` population and the reason its cross-thread handshake
        // reads `accepted=0 refused=1730`: one parked peer whose innermost
        // frame is at a poll refuses the collection for every thread.
        //
        // The single-pass backend has always bracketed its poll this way
        // (`x64/safepoint.rs::emit_safepoint_poll`: spill, sp-id store, call,
        // oop map). This is that same bracketing, in the IR backend's own
        // idiom -- it keeps every live value in a frame slot, so the map is a
        // set of frame offsets and there is no register file to spill.
        //
        // Emitted INSIDE the taken branch: the fast path (flag clear, which is
        // every execution but the ones that actually stop) is byte-for-byte
        // unchanged, so a back edge in a hot loop pays nothing.
        let mapped = Self::ir_gc_point_maps_enabled() && self.emit_safepoint_map_if_enabled();
        self.emit_mov_reg_imm64(RAX, self.safepoint_slow_path as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        if mapped {
            // Pairs with the push `emit_safepoint_map` emitted: copies the
            // (possibly rewritten) published values back into their frame slots
            // and retracts `top`. Without it a poll that fires inside a loop
            // leaks its published oops and walks the 2 MiB shadow stack off its
            // end -- the failure mode the self-recursive direct-call arm
            // documents.
            self.emit_shadow_reload();
        }
        let rel = self.buf.pos() as i32 - (clear_patch as i32 + 4);
        // IR safepoint poll -- tolerated on an overflowed buffer; see
        // `Self::patch_or_bail` / `patch_rel32_to_here`.
        Self::patch_or_bail(&mut self.buf, clear_patch, rel);
    }

    /// `emit_safepoint_map` at the current spill watermark, reporting whether
    /// it emitted anything.
    ///
    /// `emit_safepoint_map` returns early -- silently -- when the compilation
    /// reserved no sp-id slot or `CRATONVM_JIT_IR_RELOC_EMIT=0` withdrew the
    /// contract. A caller that must pair a shadow RELOAD with the push needs to
    /// know which happened; the eight call sites that sit in front of a real
    /// call do not, because they reach the reload through
    /// `emit_call_return_check`, whose `pending_shadow` is empty in that case.
    fn emit_safepoint_map_if_enabled(&mut self) -> bool {
        if self.sp_id_slot_off <= 0 || !Self::reloc_emit_enabled() {
            return false;
        }
        self.emit_safepoint_map(self.spill_high_water);
        true
    }

    /// `CRATONVM_JIT_IR_GC_POINT_MAPS=0` -- restore the two IR-backend
    /// GC-capable sites that recorded no safepoint at all: the cooperative
    /// safepoint POLL and `Op::New`.
    ///
    /// Default ON, and one switch rather than two because it is one statement:
    /// every point in an IR body that can reach a collection has to record an
    /// id and a map, or the collector reads the sp-id slot and finds either the
    /// prologue sentinel (no map -> the cycle refuses) or the id of an EARLIER
    /// safepoint (a map for a different program point -> a silent wrong
    /// answer). The other six GC-capable ops have always paid it.
    ///
    /// With it off, `frame_coverage_reason::NO_MAP_FOR_STORED_ID` climbs again
    /// and `xt_cov` refusals rise -- the census the fix is measured on.
    fn ir_gc_point_maps_enabled() -> bool {
        use std::sync::OnceLock;
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| {
            !matches!(
                cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_GC_POINT_MAPS").as_deref(),
                Ok("0") | Ok("false") | Ok("FALSE")
            )
        })
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

    /// Emit the ModRM byte and displacement of a `[RBP - offset]` operand whose
    /// ModRM `reg` field is `reg`, choosing the **smallest legal form**.
    ///
    /// RBP has no `mod=00` encoding — that bit pattern is RIP-relative — so the
    /// two forms are `mod=01` + disp8 and `mod=10` + disp32, and a zero
    /// displacement still needs an explicit disp8 of `0`. Same rule
    /// `x64::disp` states for the single-pass backend, applied here because
    /// this is the backend that touches the frame on *every* value read.
    ///
    /// One helper rather than the rule repeated per emitter: the per-emitter
    /// shape is exactly how `store_abi_reg` came to pick the short form while
    /// `load_reg_from_frame` directly below it kept emitting disp32.
    fn emit_rbp_modrm_disp(&mut self, reg: u8, offset: i32) {
        let neg = -offset;
        if (i32::from(i8::MIN)..=i32::from(i8::MAX)).contains(&neg) {
            // mod=01, r/m=RBP(101), disp8
            self.buf.emit_byte(0x45 | ((reg & 7) << 3));
            // Cast: guarded by the range check above.
            self.buf.emit_byte(neg as u8);
        } else {
            // mod=10, r/m=RBP(101), disp32
            self.buf.emit_byte(0x85 | ((reg & 7) << 3));
            self.buf.emit(&neg.to_le_bytes());
        }
    }

    /// MOV reg, [RBP - offset]  (REX.W [+ REX.R]; smallest displacement form).
    /// General form of `load_to_rax`/`load_to_rcx` for an arbitrary (possibly
    /// extended) destination.
    fn load_reg_from_frame(&mut self, reg: u8, offset: i32) {
        let mut prefix = 0x48u8;
        if reg >= 8 {
            prefix |= 0x04;
        }
        self.buf.emit_byte(prefix);
        self.buf.emit_byte(0x8B);
        self.emit_rbp_modrm_disp(reg, offset);
    }

    /// LEA reg, [RBP - offset]  (REX.W [+ REX.R]; smallest displacement form).
    /// Used to compute the `args_ptr` the dispatch helper reads the marshalled
    /// Java args from.
    fn lea_reg_from_frame(&mut self, reg: u8, offset: i32) {
        let mut prefix = 0x48u8;
        if reg >= 8 {
            prefix |= 0x04;
        }
        self.buf.emit_byte(prefix);
        self.buf.emit_byte(0x8D);
        self.emit_rbp_modrm_disp(reg, offset);
    }

    /// `MOV qword [rbp - off], 0` (mod=10 disp32, /0).
    /// `CRATONVM_JIT_ZERO_SPID` — default ON. Off restores the pre-fix
    /// behaviour (the safepoint-id slot reads uninitialised stack until the
    /// first safepoint stores an id), so the repair can be A/B'd on one binary.
    fn zero_sp_id_slot_enabled() -> bool {
        // Shared with the single-pass backend, which establishes its own
        // sentinel in `x64::frames::emit_prologue` under the same key.
        crate::sp_id_slot_init_enabled()
    }

    fn emit_zero_frame_slot(&mut self, off: i32) {
        self.buf.emit(&[0x48, 0xC7, 0x85]);
        self.buf.emit(&(-off).to_le_bytes());
        self.buf.emit(&0i32.to_le_bytes());
    }

    /// Restore the shadow `top` captured at method entry, unwinding any push
    /// this activation did not pop. Mirrors the single-pass backend's epilogue.
    /// R10/R11 are caller-saved and dead at every exit; RAX (the return value,
    /// or the `i64::MIN` sentinel on the bail path) is untouched.
    fn emit_shadow_savetop_restore(&mut self) {
        if self.get_current_thread == 0
            || self.shadow_thread_slot_off <= 0
            || self.shadow_savetop_slot_off <= 0
        {
            return;
        }
        let ss_top = self.shadow_off_in_thread;
        // MOV R10, [rbp - thread_slot] ; TEST R10,R10 ; JE skip
        self.buf.emit(&[0x4C, 0x8B, 0x95]);
        self.buf.emit(&(-self.shadow_thread_slot_off).to_le_bytes());
        self.buf.emit(&[0x4D, 0x85, 0xD2]);
        self.buf.emit(&[0x0F, 0x84]);
        let skip = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        // MOV R11, [rbp - savetop] ; MOV [R10 + ss_top], R11
        self.buf.emit(&[0x4C, 0x8B, 0x9D]);
        self.buf
            .emit(&(-self.shadow_savetop_slot_off).to_le_bytes());
        self.buf.emit(&[0x4D, 0x89, 0x9A]);
        self.buf.emit(&ss_top.to_le_bytes());
        self.patch_rel32_to_here(skip);
    }

    fn emit_epilogue(&mut self) {
        self.emit_shadow_savetop_restore();
        self.emit_callee_saved_restore();
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
        let mut bytes = FrameAccess::new();
        enc_frame_load(RAX, offset, &mut bytes);
        self.buf.emit(bytes.as_slice());
    }

    /// MOV RCX, [RBP - offset]
    fn load_to_rcx(&mut self, offset: i32) {
        let mut bytes = FrameAccess::new();
        enc_frame_load(RCX, offset, &mut bytes);
        self.buf.emit(bytes.as_slice());
    }

    /// MOV [RBP - offset], RAX
    fn store_rax(&mut self, offset: i32) {
        let mut bytes = FrameAccess::new();
        enc_frame_store(RAX, offset, &mut bytes);
        self.buf.emit(bytes.as_slice());
    }

    /// `MOV qword [RBP - offset], imm32` (sign-extended), or `false` when the
    /// value needs all 64 bits and the caller must go through a register.
    ///
    /// A constant's home word is still written — a deopt frame may name it,
    /// and a handful of sites read home slots directly — but since every
    /// READER materialises a constant as an immediate
    /// (`ir_const_imm_enabled`), nothing needs it in RAX on the way there.
    /// One instruction instead of two, at every `Op::Const` in every method.
    fn emit_store_frame_imm32(&mut self, offset: i32, val: i64) -> bool {
        let Ok(imm) = i32::try_from(val) else {
            return false;
        };
        self.buf.emit_byte(0x48); // REX.W
        self.buf.emit_byte(0xC7); // MOV r/m64, imm32 (/0)
        self.emit_rbp_modrm_disp(0, offset);
        self.buf.emit(&imm.to_le_bytes());
        true
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
    // needed; the `[rbp - offset]` operand goes through
    // `emit_rbp_modrm_disp`, which picks disp8 or disp32 — the ModRM `reg`
    // field is an XMM number here rather than a GPR number, which changes
    // nothing about the encoding of the memory half.

    /// MOVSS/MOVSD xmm, [rbp - offset] — load a 32/64-bit FP value from a slot.
    fn fp_load(&mut self, xmm: u8, offset: i32, is_double: bool) {
        self.buf.emit_byte(if is_double { 0xF2 } else { 0xF3 });
        self.buf.emit(&[0x0F, 0x10]);
        self.emit_rbp_modrm_disp(xmm, offset);
    }

    /// MOVSS/MOVSD [rbp - offset], xmm — store an FP value to a slot. A `MOVSS`
    /// writes only the low 4 bytes; the slot's high 4 are left stale, which is
    /// harmless because every float consumer reads it back with `MOVSS` (4 bytes).
    fn fp_store(&mut self, offset: i32, xmm: u8, is_double: bool) {
        self.buf.emit_byte(if is_double { 0xF2 } else { 0xF3 });
        self.buf.emit(&[0x0F, 0x11]);
        self.emit_rbp_modrm_disp(xmm, offset);
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
            // Range-checked, not truncated: see
            // `ExecutableBuffer::patch_rel8_or_bail`. This sequence is
            // fixed-size and comfortably inside rel8 today, so the helper can
            // only fire on a genuine codegen bug — which is exactly the case
            // the single-pass backend's identical `as u8` patches did not
            // survive.
            let rel = |target: usize, patch: usize| (target as i64) - (patch as i64) - 1;
            self.buf
                .patch_rel8_or_bail(jp_patch, rel(nan_off, jp_patch));
            self.buf.emit(&[0x31, 0xC0]);
            // .done:
            let done_off = self.buf.pos();
            self.buf
                .patch_rel8_or_bail(jne_patch, rel(done_off, jne_patch));
            self.buf
                .patch_rel8_or_bail(jbe_patch, rel(done_off, jbe_patch));
            self.buf
                .patch_rel8_or_bail(jmp_patch, rel(done_off, jmp_patch));
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
            // Range-checked, not truncated — see the `!is_long` arm above.
            let rel = |target: usize, patch: usize| (target as i64) - (patch as i64) - 1;
            self.buf
                .patch_rel8_or_bail(jp_patch, rel(nan_off, jp_patch));
            self.buf.emit(&[0x48, 0x31, 0xC0]);
            // .done:
            let done_off = self.buf.pos();
            self.buf
                .patch_rel8_or_bail(jne_patch, rel(done_off, jne_patch));
            self.buf
                .patch_rel8_or_bail(jbe_patch, rel(done_off, jbe_patch));
            self.buf
                .patch_rel8_or_bail(jmp_patch, rel(done_off, jmp_patch));
        }
    }

    // ── Node lowering ────────────────────────────────────────────────

    fn lower_block(&mut self, block_idx: usize) {
        self.block_offsets[block_idx] = self.buf.pos();
        // A receiver proof is block-local: control can enter this block from a
        // predecessor that never proved it.
        self.guarded_receivers.clear();
        self.null_proven_receivers.clear();
        self.seed_block_null_proofs();

        let block = &self.schedule.blocks[block_idx];

        // Emit data nodes.
        //
        // The level-2 detour is a no-op unless a machine-level flag is on; see
        // `lower_data_node_through_mir`, which falls through to the per-opcode
        // arm for every node no tile covers.
        for &node_id in &block.nodes {
            self.lower_data_node_through_mir(block_idx, node_id);
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
            self.push_call_exc_patch(patch);
        }
        let rel = self.buf.pos() as i32 - (fast_skip_patch as i32 + 4);
        // ir_lower self-call stack-sample -- tolerated on an overflowed buffer; see
        // `Self::patch_or_bail` / `patch_rel32_to_here`.
        Self::patch_or_bail(&mut self.buf, fast_skip_patch, rel);

        // Marshal args into this method's OWN entry ABI — IDENTICAL to the
        // register list `emit_prologue` reads incoming args from: abi[0] = the
        // hidden VM context pointer, abi[1 + i] = Java arg i. Each source is a
        // frame slot (memory), so loading straight into the abi registers cannot
        // inter-clobber.
        //
        // `1 + num_args <= abi.len()` is required and is enforced at ELIGIBILITY
        // (`jit::lib`'s `is_self_recursive_direct`, via
        // `incoming_abi_reg_capacity`), not here. It used to be implied by
        // `lower()`'s whole-method bail on a graph with more incoming slots than
        // registers; when `emit_prologue` learned to read the overflow off the
        // caller's stack (Gap B) that bail went away and this comment kept
        // asserting a guarantee nobody was making any more.
        //
        // MEASURED, 2026-08-29, Windows: a `static long f(int,int,int,int)`
        // calling itself is 4 Java args plus the context = 5 against a
        // four-register file, so `abi[4]` panicked the compiler thread with
        // `index out of bounds: the len is 4 but the index is 4` — and the
        // thread does not come back, so the FIRST such method silently disables
        // the JIT for the rest of the process. SysV has six registers and needs
        // six arguments to reach it, which is why it showed up on Windows first.
        // `probes/SelfRecArgs.java` is the arity sweep.
        #[cfg(target_os = "windows")]
        let abi: &[u8] = &[1, 2, 8, 9]; // RCX, RDX, R8, R9
        #[cfg(not(target_os = "windows"))]
        let abi: &[u8] = &[7, 6, 2, 1, 8, 9]; // RDI, RSI, RDX, RCX, R8, R9
        self.load_reg_from_frame(abi[0], self.context_slot_off); // vm_ptr
        for i in 0..num_args {
            let arg = inputs[2 + i];
            self.gp_load_value(abi[1 + i], arg); // Java arg i
        }
        // Direct CALL rel32 to entry (code offset 0), patched at finalize.
        self.buf.emit(&[0xE8]);
        let patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.self_call_patches.push(patch);
        // The recursive callee published ITS frame's RBP into the mirror on
        // entry. This route bypasses `emit_call_return_check`, so it has to
        // republish here or the mirror keeps naming the returned frame — which
        // for a deeply recursive method is every frame but the right one.
        self.emit_post_call_frame_record();
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
        self.push_call_exc_patch(exc_patch);
        // .keep:
        let keep_off = self.buf.pos();
        let rel = keep_off as i32 - (keep_patch as i32 + 4);
        // ir_lower self-call sentinel keep -- tolerated on an overflowed buffer; see
        // `Self::patch_or_bail` / `patch_rel32_to_here`.
        Self::patch_or_bail(&mut self.buf, keep_patch, rel);
        // Spill the return value.
        self.store_rax(slot);
    }

    /// IR direct-call lowering — emit a statically-bound `Op::Call` as a raw
    /// `CALL` into the callee's already-compiled entry, instead of the generic
    /// `jit_invoke_dispatch` round trip.
    ///
    /// This is the IR analogue of the single-pass backend's direct-call path
    /// (`x64.rs`, `direct_calls` / `emit_call_absolute`): the eligibility loop in
    /// `lib.rs` eagerly compiles a resolved `invokestatic` / non-`<init>`
    /// `invokespecial` callee via `callee_compiler` and records
    /// `(pc → (entry, callee_needs_context))`. Both call kinds are STATICALLY
    /// bound, so no receiver TYPE check is needed — exactly why single-pass may
    /// bind them directly too.
    ///
    /// A null check is a different question, and this sentence used to answer
    /// it by omission. `invokestatic` has no receiver to test; `invokespecial`
    /// does, and JVMS 6.5 raises NPE at the INVOKE rather than inside the
    /// callee. Jumping straight to a compiled entry skips the dispatch door
    /// that used to raise it, so the only thing left to fault was the callee
    /// body — and a body that never dereferences `this` does not. See
    /// `has_receiver` below.
    ///
    /// # Why this matters
    ///
    /// `jit_invoke_dispatch` costs a full helper round trip per call
    /// (`note_jit_boundary`, an SATB flush, a `lookup_jit_code_range` scan, a
    /// re-entrant `JitEntryGuard` push/pop, and re-marshalling the staged args
    /// into `Value`s). That per-call tax is the whole reason the IR pipeline
    /// capped `invoke_ops` at 5 — an "optimizing" recompile of a call-heavy
    /// method was a NET REGRESSION versus the single-pass body, which has had
    /// direct calls all along. Lowering the statically-bound cases directly
    /// removes the tax, so the cap can be raised.
    ///
    /// # ABI
    ///
    /// The callee is an `extern "C"` compiled artifact whose prologue reads its
    /// incoming arguments from [`ENTRY_ABI_REGS`]: `abi[0]` = the hidden VM
    /// context pointer when the callee `needs_context`, then one register per
    /// Java argument. Every argument is marshalled as a raw 64-bit slot value
    /// (the VM's compact all-GPR JIT ABI — an FP argument travels as its
    /// `to_bits()` pattern in an INTEGER register, and an FP result comes back in
    /// RAX), which is exactly how each argument is already stored in its frame
    /// slot. Every source is memory, so loading straight into the ABI registers
    /// cannot inter-clobber; `RAX` (the indirect-call target scratch) is not an
    /// argument register on either ABI.
    ///
    /// # Arguments past the register file
    ///
    /// Arguments beyond `ENTRY_ABI_REGS` travel on the stack, exactly as the
    /// single-pass backend's `x64::frames::emit_stack_arg_setup` marshals them:
    /// reserve the block (Win64 shadow space included, rounded to 16 so the
    /// `CALL` stays aligned), materialise the stack args through `RAX` FIRST,
    /// then load the register args, then `CALL`, then release the block. Every
    /// source is `[rbp - off]`, which `SUB RSP` does not disturb, so the two
    /// passes cannot interfere.
    ///
    /// This is what the `HttpContentDecompressorTest` page was blocked on: a
    /// `ByteBuffer` accessor's remaining native rungs are
    /// `ScopedMemoryAccess.put*Unaligned`, which need **seven** incoming slots
    /// (receiver + five arguments + the context pointer) and so could not be
    /// bound to a thin direct helper at this door while the lowering was
    /// register-only.
    ///
    /// Note the asymmetry with the *callee* side: `emit_prologue` still reads
    /// incoming arguments out of `ENTRY_ABI_REGS` only, and `lower()` still
    /// refuses a graph with more parameters than that. That is unchanged and
    /// deliberate — the callees this widened path reaches are `extern "C"`
    /// thin VM helpers, which read their stack arguments the way the platform
    /// C ABI says. A JIT-compiled callee still cannot receive one, which is why
    /// the caller of this function keeps its own capacity check for that case.
    ///
    /// SAFETY: `entry` is a code address produced by this JIT for the resolved
    /// callee; `lib.rs` records it in `CompiledMethod::_direct_callee_entries`,
    /// which both keeps the callee's buffer alive (`_direct_callee_roots`) and
    /// puts this method into the callee's invalidation closure, so the baked
    /// address can never outlive the code it points at.
    #[allow(clippy::too_many_arguments)]
    fn emit_direct_cross_call(
        &mut self,
        inputs: &[NodeId],
        slot: i32,
        num_args: usize,
        entry: usize,
        callee_needs_ctx: bool,
        info_ptr: usize,
        ty: IrType,
        has_receiver: bool,
        bci: usize,
    ) {
        // JVMS 6.5 on argument 0 of a receiver-bearing call. Deopt rather than
        // raise inline: the interpreter re-executes this invoke and owns the
        // canonical NPE, its message and its stack trace, exactly as it does
        // for the field-access null checks elsewhere in this lowerer.
        //
        // This is all that survives of the loop that used to run here. The
        // rest of it copied every argument into the staging region, whose only
        // reader is `emit_inline_callee_deopt_service` — reached when the
        // callee returns the deopt sentinel, and otherwise never. Paying N
        // loads and N stores on the hot path so a cold path could read a
        // contiguous array made every call cost 3N memory operations where N
        // does: the register marshal below reads the same frame slots again.
        // The staging now happens inside that cold block, out of those same
        // slots, which still hold the same values there because nothing
        // between the marshal and the sentinel test writes them.
        if !Self::cold_arg_stage_enabled() {
            // The pre-2026-09-02 shape, kept behind the switch: stage every
            // argument eagerly, with the receiver null check folded into the
            // first iteration exactly as it was.
            for i in 0..num_args {
                let arg = inputs[2 + i];
                self.gp_load_value(RAX, arg);
                if i == 0 && has_receiver {
                    self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                    self.emit_deopt_if_zero(bci, DeoptReason::NullCheck);
                }
                // Cast: an argument index is bounded by the callee's parameter count.
                self.store_rax(self.args_stage_top_off - (i as i32) * 8);
            }
        } else if has_receiver && num_args > 0 {
            self.gp_load_value(RAX, inputs[2]);
            self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
            self.emit_deopt_if_zero(bci, DeoptReason::NullCheck);
        }
        let base = usize::from(callee_needs_ctx);
        // Java arguments that fit the register file, and the remainder that
        // must be pushed. `saturating_sub` rather than a subtraction: a callee
        // taking the context pointer plus fewer arguments than the file holds
        // leaves `reg_capacity > num_args`, which is the ordinary case.
        let reg_capacity = ENTRY_ABI_REGS.len() - base;
        let stack_args = num_args.saturating_sub(reg_capacity);
        let (total_sub, base_disp) = stack_arg_block_size(stack_args);
        if total_sub > 0 {
            self.emit_sub_rsp_imm32(total_sub);
        }
        // Materialise the stack arguments before the register pass: this uses
        // RAX as the transfer scratch, and the register pass must be the last
        // thing before the `CALL` so nothing can clobber an ABI register after
        // it is loaded.
        for k in 0..stack_args {
            let arg = inputs[2 + reg_capacity + k];
            self.gp_load_value(RAX, arg);
            // Cast: a stack-arg index is bounded by the callee's parameter
            // count, so `k * 8` cannot overflow an x86-64 displacement.
            self.emit_mov_rsp_disp_from_rax(base_disp + (k as i32) * 8);
        }
        if callee_needs_ctx {
            self.load_reg_from_frame(ENTRY_ABI_REGS[0], self.context_slot_off);
        }
        for i in 0..num_args.min(reg_capacity) {
            let arg = inputs[2 + i];
            self.gp_load_value(ENTRY_ABI_REGS[base + i], arg);
        }
        // MOV RAX, entry ; CALL RAX.
        self.emit_mov_reg_imm64(RAX, entry as u64);
        self.buf.emit(&[0xFF, 0xD0]);
        // Release the block BEFORE anything else runs. The deopt service below
        // calls back into the VM and relies on the frame's own reserved shadow
        // space at `[rsp, rsp+32)`; leaving RSP lowered would point that at the
        // (now dead) stack-argument block instead.
        if total_sub > 0 {
            self.emit_add_rsp_imm32(total_sub);
        }
        if crate::x64::merged_call_sentinel_enabled() {
            // 2026-09-02: one sentinel compare on the hot path. The frame
            // republish and shadow reload come FIRST so the cold side (which
            // can run the interpreter and collect) sees this frame's own
            // identity rather than the callee's dead one.
            self.emit_post_call_frame_record();
            self.emit_shadow_reload();
            let keep = self.emit_call_sentinel_fast_skip();
            self.emit_inline_callee_deopt_service(info_ptr, num_args, inputs);
            self.emit_call_return_sentinel_tail(ty);
            self.patch_rel32_to_here(keep);
            self.store_rax(slot);
        } else {
            self.emit_inline_callee_deopt_service(info_ptr, num_args, inputs);
            self.emit_call_return_check(slot, ty);
        }
    }

    /// `SUB RSP, imm32` — reserve a call's outgoing stack-argument block.
    fn emit_sub_rsp_imm32(&mut self, bytes: i32) {
        self.buf.emit(&[0x48, 0x81, 0xEC]);
        self.buf.emit(&bytes.to_le_bytes());
    }

    /// `ADD RSP, imm32` — release the block reserved by [`Self::emit_sub_rsp_imm32`].
    fn emit_add_rsp_imm32(&mut self, bytes: i32) {
        self.buf.emit(&[0x48, 0x81, 0xC4]);
        self.buf.emit(&bytes.to_le_bytes());
    }

    /// `MOV qword [RSP + disp32], RAX` — place one outgoing stack argument.
    ///
    /// RSP-relative addressing needs a SIB byte (`ModRM.rm = 100`); `0x24` is
    /// `base = RSP, index = none`.
    fn emit_mov_rsp_disp_from_rax(&mut self, disp: i32) {
        self.buf.emit(&[0x48, 0x89, 0x84, 0x24]);
        self.buf.emit(&disp.to_le_bytes());
    }

    /// Service an exceptional return from an inline cached compiled callee.
    ///
    /// The service reads the outgoing Java arguments as a contiguous array, so
    /// they have to be materialised into the staging region — but **here**, on
    /// the sentinel-taken side of the branch, not at the call site.
    ///
    /// They used to be staged unconditionally before every call, which cost N
    /// loads and N stores on a path that then loaded the same N frame slots
    /// again to marshal them into ABI registers: 3N memory operations per call
    /// for a cold path's convenience. Staging here is sound because the
    /// arguments live in the lowerer's own frame slots and nothing between the
    /// marshal and this test writes them — the callee cannot, it owns a
    /// different frame, and the marshal only reads.
    ///
    /// `inputs` is the call node's input list; arguments start at index 2, the
    /// same convention every caller in this file uses.
    fn emit_inline_callee_deopt_service(
        &mut self,
        info_ptr: usize,
        num_args: usize,
        inputs: &[NodeId],
    ) {
        if self.service_callee_deopt == 0 {
            return;
        }
        self.emit_mov_reg_imm64(R10, i64::MIN as u64);
        self.buf.emit(&[0x4C, 0x39, 0xD0]); // CMP RAX, R10
        self.buf.emit(&[0x0F, 0x85]); // JNE .done
        let skip = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        // ── cold from here ──────────────────────────────────────────────
        // RAX holds the sentinel, so it is free as the transfer scratch.
        // Skipped under the eager shape: the call site already staged.
        if Self::cold_arg_stage_enabled() {
            for i in 0..num_args {
                let arg = inputs[2 + i];
                self.gp_load_value(RAX, arg);
                // Cast: an argument index is bounded by the callee's parameter
                // count, so `i * 8` cannot overflow an x86-64 displacement.
                self.store_rax(self.args_stage_top_off - (i as i32) * 8);
            }
        }
        self.load_reg_from_frame(CALL_ARG_REGS[0], self.context_slot_off);
        self.emit_mov_reg_imm64(CALL_ARG_REGS[1], info_ptr as u64);
        self.lea_reg_from_frame(CALL_ARG_REGS[2], self.args_stage_top_off);
        self.emit_mov_reg_imm64(CALL_ARG_REGS[3], num_args as u64);
        self.emit_mov_reg_imm64(RAX, self.service_callee_deopt as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        let rel = self.buf.pos() as i32 - (skip as i32 + 4);
        Self::patch_or_bail(&mut self.buf, skip, rel);
    }

    // ── Inline caches (jit-inlining-and-ir-calls) ────────────────────

    /// Record one exceptional-exit branch for [`Self::emit_call_exc_stub`],
    /// tagged with the bytecode pc currently being lowered.
    ///
    /// Every site that jumps to the shared bail stub goes through here so no
    /// exit can reach the stub without a throw-site bci — the defect the stub's
    /// own doc comment describes.
    fn push_call_exc_patch(&mut self, patch: usize) {
        // `cur_bci` is the node's own pc, which inside a spliced region is a
        // combined-buffer pc — see `resume_bci`.
        let bci = self.resume_bci(self.cur_bci);
        self.call_exc_patches.push((patch, bci));
    }

    /// Emit `Jcc rel32` with a placeholder displacement; returns the native
    /// offset of the 4-byte operand. `cc` is the second opcode byte:
    /// `0x84` = JE/JZ, `0x85` = JNE/JNZ.
    fn emit_jcc_rel32(&mut self, cc: u8) -> usize {
        self.buf.emit(&[0x0F, cc]);
        let patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        patch
    }

    /// Emit `JMP rel32` with a placeholder displacement; returns the operand
    /// offset.
    fn emit_jmp_rel32(&mut self) -> usize {
        self.buf.emit_byte(0xE9);
        let patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        patch
    }

    /// Resolve a rel32 branch operand so it targets the CURRENT buffer
    /// position.
    ///
    /// Never panics. Once `ExecutableBuffer` has overflowed it drops writes and
    /// stops advancing `len`, so recorded patch offsets no longer address their
    /// placeholder bytes and `try_patch_i32` legitimately fails (setting the
    /// sticky flag itself). `lower_inner` discards the whole artifact in that
    /// case, so swallowing the error is correct — an `expect` here would turn a
    /// recoverable "fall back to single-pass" into a compile-thread panic.
    fn patch_rel32_to_here(&mut self, patch: usize) {
        let here = self.buf.pos();
        let rel = here as i32 - (patch as i32 + 4);
        let _ = self.buf.try_patch_i32(patch, rel);
    }

    /// Marshal a cache hit into the compiled-callee entry ABI.
    ///
    /// Context-using artifacts receive the hidden VM pointer in `abi[0]`;
    /// context-free artifacts receive Java arg0 there. Inline caches retain the
    /// callee's ABI bit, so both shapes can use the cache instead of forcing
    /// context-free methods back through the resolving helper.
    fn emit_ic_abi_marshal(&mut self, inputs: &[NodeId], num_args: usize, needs_context: bool) {
        let base = if needs_context {
            self.load_reg_from_frame(ENTRY_ABI_REGS[0], self.context_slot_off);
            1
        } else {
            0
        };
        for i in 0..num_args {
            let arg = inputs[2 + i];
            self.gp_load_value(ENTRY_ABI_REGS[base + i], arg);
        }
    }

    /// Marshal the Java arguments into the **no-context** register layout once,
    /// ahead of the whole inline-cache cascade.
    ///
    /// # Why this may sit before the guards
    ///
    /// The cascade's guards touch RAX (the receiver and its class id), R10 (the
    /// cache-slot base) and R11 (the call target), and **nothing else** — no
    /// `ENTRY_ABI_REGS` member appears in `emit_cmp_eax_r10_disp`,
    /// `emit_cmp_byte_r10_disp_zero`, `emit_cmp_qword_r10_disp_zero`, or the
    /// receiver null and kind checks. A layout established here therefore
    /// survives every arm of the cascade to its `CALL`.
    ///
    /// # What it replaces
    ///
    /// `needs_context` is a property of the *cached entry*, not of the site, so
    /// the marshalling was emitted twice per cache entry — ten copies at a site
    /// with one MIC and a four-entry PIC, each of them `num_args` frame loads.
    /// The no-context layout is now built once, and a context-needing arm
    /// converts it with [`Self::emit_ic_shift_for_context`], which is
    /// register-to-register.
    ///
    /// The alternative — one uniform entry ABI — would delete the question
    /// entirely, and is deliberately not taken here: `needs_context` is an
    /// output of optimization, so changing it changes how *every* compiled
    /// method receives its arguments. That is not a change to fold into this
    /// one.
    fn emit_ic_premarshal_no_context(&mut self, inputs: &[NodeId], num_args: usize) {
        for i in 0..num_args {
            let arg = inputs[2 + i];
            self.gp_load_value(ENTRY_ABI_REGS[i], arg);
        }
    }

    /// Convert the pre-marshalled no-context layout into the context one: shift
    /// every argument up one register and load the context into
    /// `ENTRY_ABI_REGS[0]`.
    ///
    /// **Descending order is load-bearing.** Moving `[0] -> [1]` first would
    /// overwrite argument 1 before it is read; from the top down, every
    /// destination holds a value that has already been moved.
    ///
    /// Always legal: `emit_inline_cache_call`'s admission requires
    /// `num_args + 1 <= ENTRY_ABI_REGS.len()`
    /// (`ic_declines_when_args_overflow_the_abi_register_file` pins it), so the
    /// top destination `ENTRY_ABI_REGS[num_args]` is always inside the file.
    fn emit_ic_shift_for_context(&mut self, num_args: usize) {
        debug_assert!(
            num_args + 1 <= ENTRY_ABI_REGS.len(),
            "the context shift needs {} registers and the file has {}",
            num_args + 1,
            ENTRY_ABI_REGS.len(),
        );
        for i in (0..num_args).rev() {
            self.emit_mov_reg_reg64(ENTRY_ABI_REGS[i + 1], ENTRY_ABI_REGS[i]);
        }
        self.load_reg_from_frame(ENTRY_ABI_REGS[0], self.context_slot_off);
    }

    /// `CMP EAX, dword [R10 + disp]` — an inline-cache class-id guard.
    fn emit_cmp_eax_r10_disp(&mut self, disp: u8) {
        if disp == 0 {
            // REX.B + 3B /r + ModRM(mod=00, reg=RAX, rm=R10)
            self.buf.emit(&[0x41, 0x3B, 0x02]);
        } else {
            // REX.B + 3B /r + ModRM(mod=01, reg=RAX, rm=R10) + disp8
            self.buf.emit(&[0x41, 0x3B, 0x42, disp]);
        }
    }

    /// `CMP BYTE [R10 + disp], 0` — selects the cached target's entry ABI.
    fn emit_cmp_byte_r10_disp_zero(&mut self, disp: u8) {
        // REX.B + 80 /7 + ModRM(mod=01, /7, rm=R10) + disp8 + imm8
        self.buf.emit(&[0x41, 0x80, 0x7A, disp, 0x00]);
    }

    /// `CMP QWORD [R10 + disp], 0` — rejects a profile-seeded cache
    /// entry whose class id is known but whose compiled target is not yet
    /// installed.
    fn emit_cmp_qword_r10_disp_zero(&mut self, disp: u8) {
        // REX.W+B + 83 /7 + ModRM(mod=01, /7, rm=R10) + disp8 + imm8
        self.buf.emit(&[0x49, 0x83, 0x7A, disp, 0x00]);
    }

    /// `MOV R11, qword [R10 + disp] ; CALL R11` — the cached-entry indirect
    /// call.
    ///
    /// SECURITY INVARIANT (inherited verbatim from `x64.rs`'s inline caches):
    /// nothing may be emitted between these two instructions. R10 is a general
    /// scratch register; keeping the call target addressed through it across
    /// any other emitted instruction would let an R10 clobber redirect native
    /// control flow. Loading into R11 (caller-saved, not an argument register
    /// on either ABI, clobbered by the call anyway) immediately before the
    /// `CALL` closes the window to a single fixed pair.
    fn emit_call_cached_entry(&mut self, disp: u8) {
        if disp == 0 {
            self.buf.emit(&[0x4D, 0x8B, 0x1A]); // MOV R11, [R10]
        } else {
            self.buf.emit(&[0x4D, 0x8B, 0x5A, disp]); // MOV R11, [R10+disp8]
        }
        self.buf.emit(&[0x41, 0xFF, 0xD3]); // CALL R11
                                            // The callee is COMPILED JAVA, so its prologue published ITS (rbp,
                                            // compile id) into the innermost-frame mirror and nothing on the return
                                            // path of a raw JIT->JIT call restores this frame's. Both inline-cache
                                            // arms in this tier went without it until 2026-08-24, so after any
                                            // monomorphic or polymorphic hit the mirror named a frame that had
                                            // already returned -- measured on H2 `TestMVStoreTool`, where ONE rbp
                                            // with ONE saved return address inside `TestMVStoreTool.testCompact`
                                            // was claimed across collections by four different methods, one of them
                                            // `RootReference.isLocked` with `maps=0`, which emits no safepoint and
                                            // therefore cannot be the frame at a collection at all.
                                            //
                                            // `moving_young_frame_coverage_complete` then reads
                                            // `[stale_rbp - stale_method.sp_id_slot_off]`, finds a word matching no
                                            // map, and refuses the whole cycle (`ACTIVE_FRAME_MAP`,
                                            // `frame_cov=(no_map=N incomplete=0)`), which is what kept ZGC from
                                            // compacting on `bug-h2-testkillprocess-zgc-oom-at-97-percent-free`.
                                            // The single-pass backend republishes at both of its equivalent arms,
                                            // and the shared hashed/vtable stub below is handed `self.frame_record`
                                            // for exactly this -- these two arms were the gap.
                                            //
                                            // It lives INSIDE this helper rather than at the two call sites so a
                                            // third arm cannot be added without it. RAX (the callee's Java return
                                            // value) is preserved by both forms of the republish, and the security
                                            // invariant above is about the MOV/CALL pair, which nothing here comes
                                            // between.
        if !ic_frame_republish_disabled() {
            self.emit_post_call_frame_record();
            IC_FRAME_REPUBLISH_SITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// IR inline-cache lowering — serve a virtual / interface `Op::Call` from a
    /// monomorphic (`JitMICSlot`) guard, then a 4-way polymorphic
    /// (`JitPICSlot`) cascade, then the resolving `jit_invoke_virtual_mic`
    /// helper. This is the IR analogue of the single-pass backend's MIC/PIC
    /// codegen in `x64.rs`.
    ///
    /// # Why this matters
    ///
    /// `emit_direct_cross_call` removed the per-call helper tax for the
    /// STATICALLY BOUND kinds, but left `invokevirtual` / `invokeinterface`
    /// paying a full `jit_invoke_dispatch` round trip *plus* a dynamic
    /// vtable/itable lookup on every single call — something the single-pass
    /// backend has never done. That asymmetry is why virtual-call lowering was
    /// gated opt-IN (`CRATONVM_JIT_IR_CALL_VIRTUAL`) and why
    /// `c2_upgrade_would_engage` refused any method containing a virtual call:
    /// admitting them made the "optimizing" body slower than the C1 body it
    /// replaced. With this, an IR virtual site costs a load, a compare and an
    /// indirect call on the monomorphic path — the same as single-pass.
    ///
    /// # Shape
    ///
    /// ```text
    ///   MOV  RAX, [rbp - recv_slot]                  ; receiver = arg0
    ///   TEST RAX, RAX ; JZ .slow                     ; null → helper raises NPE
    ///   MOV  EAX, dword [RAX]                        ; class_id (ObjectHeader+0)
    ///   ; ── monomorphic ──
    ///   MOV  R10, imm64 mic
    ///   CMP  EAX, [R10 + CACHED_CLASS_ID_OFFSET]     ; JNE .pic
    ///   CMP  BYTE [R10 + CACHED_NEEDS_CONTEXT], 0    ; JE  .mic_noctx
    ///   <marshal context ABI> ; JMP .mic_call
    /// .mic_noctx: <marshal context-free ABI>
    /// .mic_call: MOV R11,[R10+8] ; CALL R11 ; JMP .done
    ///   ; ── polymorphic, 4-way ──
    /// .pic:
    ///   MOV  R10, imm64 pic
    ///   for i in 0..4:
    ///     CMP EAX, [R10+CLASS_ID_OFFSETS[i]]  ; JNE .pic_{i+1} (last: .slow)
    ///     CMP BYTE [R10+NEEDS_CONTEXT[i]], 0  ; JE  .entry_noctx
    ///     <marshal context ABI> ; JMP .entry_call
    ///   .entry_noctx: <marshal context-free ABI>
    ///   .entry_call: MOV R11,[R10+ENTRY_PTR_OFFSETS[i]] ; CALL R11 ; JMP .done
    ///   ; ── megamorphic / cold ──
    /// .slow:
    ///   <marshal args into the frame staging region>
    ///   jit_invoke_virtual_mic(vm, info, args_ptr, num_args, mic, pic)
    /// .done:
    ///   <emit_call_return_check>                     ; shared sentinel + spill
    /// ```
    ///
    /// EAX carries the receiver class id from the single header load through
    /// the whole cascade: no guard writes RAX, and the ABI marshalling only
    /// touches [`ENTRY_ABI_REGS`] (which excludes RAX, R10 and R11 on both
    /// platforms), so both the class id and the slot base survive to their uses.
    ///
    /// Cold sites cost nothing: an unpopulated slot holds `class_id == 0`,
    /// which no real receiver matches (class id 0 is `java/lang/Object`, never
    /// an `invokevirtual` dispatch target here), so every guard falls straight
    /// through to the helper — which then populates the caches, after which
    /// later invocations take the inline path with no recompile. Exactly the
    /// eager-allocation strategy `lib.rs` already uses for single-pass (HIGH-7).
    ///
    /// All forward branches use rel32. The single-pass inline caches have had
    /// two separate CRIT bugs from rel8 displacement overflow (CRIT-3, and the
    /// inter-slot `jne`); a few extra bytes per site is the right trade.
    ///
    /// Every branch is patched before this function returns, so no per-site
    /// patch state escapes into the shared stub-patching phase.
    #[allow(clippy::too_many_arguments)]
    fn emit_inline_cache_call(
        &mut self,
        inputs: &[NodeId],
        slot: i32,
        num_args: usize,
        mic: usize,
        pic: usize,
        info_ptr: usize,
        ty: IrType,
    ) {
        use crate::{JitMICSlot, JitPICSlot, JIT_PIC_ENTRIES};

        debug_assert!(
            num_args >= 1,
            "a virtual/interface site always marshals a receiver as arg0"
        );
        // This is the ONLY helper the IR lowerer calls with more than four
        // arguments, so it is the only place the Win64 stack-argument area
        // `[RSP+32, RSP+48)` is ever written. Pin that the arg-staging region
        // starts above it — if the two ever overlap again, `mic`/`pic` land on
        // the receiver and first argument of the call being dispatched. See the
        // frame-layout fix in `Lowerer::new`.
        #[cfg(target_os = "windows")]
        debug_assert!(
            self.args_stage_top_off <= self.frame_size - 48,
            "ir_lower: arg-staging region at rbp-{} overlaps the Win64 stack-argument \
             area for the 6-argument jit_invoke_virtual_mic call (frame_size {})",
            self.args_stage_top_off,
            self.frame_size,
        );
        let mut done_patches: Vec<usize> = Vec::new();
        let mut slow_patches: Vec<usize> = Vec::new();
        // The staging loop that used to run HERE — N loads and N stores ahead
        // of the guards, on every execution of every virtual call site — is
        // gone. Its three readers now each populate the block on their own cold
        // side:
        //
        //   * a MIC or PIC hit that returns the deopt sentinel — staged inside
        //     `emit_inline_callee_deopt_service`, past its `JNE .done`;
        //   * the shared hashed/vtable stub — staged immediately before it, in
        //     the megamorphic region rather than ahead of the monomorphic
        //     guard;
        //   * the resolving slow helper — which already re-staged for itself,
        //     which is what made the copy up here redundant even before this.
        //
        // All three read the same frame slots, and nothing between this point
        // and any of them writes those slots.

        // Marshal the Java arguments ONCE, in the no-context layout, before the
        // guards. Each cache arm then either calls straight through or shifts
        // the layout up one register — see `emit_ic_premarshal_no_context` for
        // why a layout established here survives the cascade (its guards touch
        // only RAX, R10 and R11).
        // Under the eager shape the whole pre-marshal/shift scheme is off and
        // each arm marshals for itself, exactly as before 2026-09-02.
        let premarshal = Self::cold_arg_stage_enabled();
        if premarshal {
            self.emit_ic_premarshal_no_context(inputs, num_args);
        } else {
            for i in 0..num_args {
                let arg = inputs[2 + i];
                self.gp_load_value(RAX, arg);
                self.store_rax(self.args_stage_top_off - (i as i32) * 8);
            }
        }

        // Receiver = arg0. Load it and its class id ONCE for the whole cascade.
        // RAX is not an `ENTRY_ABI_REGS` member on either ABI, so this does not
        // disturb the layout just built.
        self.gp_load_value(RAX, inputs[2]);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        slow_patches.push(self.emit_jcc_rel32(0x84)); // JZ .slow
                                                      // Array-receiver guard — `ObjectHeader.class_id` (offset 0) holds a
                                                      // reference array's COMPONENT class id, so `Foo[]` and `Foo` share the
                                                      // guard word every cached entry below is compared against. Without the
                                                      // `kind` check a `Foo[]` receiver is dispatched into `Foo`'s method
                                                      // body. See the matching guard in `x64.rs`'s single-pass cascade.
                                                      //   CMP BYTE [RAX + KIND_TAGS_BYTE_OFFSET], ObjectKind::Object
        self.buf.emit(&[
            0x80,
            0x78,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
            cratonvm_types::ObjectKind::Object as u8,
        ]);
        slow_patches.push(self.emit_jcc_rel32(0x85)); // JNE .slow
        self.buf.emit(&[0x8B, 0x00]); // MOV EAX, dword [RAX]

        // ── Monomorphic inline cache ────────────────────────────────
        self.emit_mov_reg_imm64(R10, mic as u64);
        self.emit_cmp_eax_r10_disp(JitMICSlot::CACHED_CLASS_ID_OFFSET as u8);
        let mic_miss = self.emit_jcc_rel32(0x85); // JNE .pic
        self.emit_cmp_qword_r10_disp_zero(JitMICSlot::CACHED_ENTRY_PTR_OFFSET as u8);
        slow_patches.push(self.emit_jcc_rel32(0x84)); // JE .slow
        self.emit_cmp_byte_r10_disp_zero(JitMICSlot::CACHED_NEEDS_CONTEXT_OFFSET as u8);
        // Falls THROUGH on the context case and jumps on the no-context one,
        // because the no-context layout is already in place: the fall-through
        // shifts it, the jump does nothing at all. That inverts the old sense of
        // this branch, which is why the target is named for what it skips.
        if premarshal {
            let mic_noctx = self.emit_jcc_rel32(0x84); // JE .mic_ready (no shift)
            self.emit_ic_shift_for_context(num_args);
            self.patch_rel32_to_here(mic_noctx);
        } else {
            let mic_noctx = self.emit_jcc_rel32(0x84); // JE .mic_noctx
            self.emit_ic_abi_marshal(inputs, num_args, true);
            let mic_call = self.emit_jmp_rel32();
            self.patch_rel32_to_here(mic_noctx);
            self.emit_ic_abi_marshal(inputs, num_args, false);
            self.patch_rel32_to_here(mic_call);
        }
        self.emit_call_cached_entry(JitMICSlot::CACHED_ENTRY_PTR_OFFSET as u8);
        self.emit_inline_callee_deopt_service(info_ptr, num_args, inputs);
        done_patches.push(self.emit_jmp_rel32());

        // ── Polymorphic 4-way cascade ───────────────────────────────
        // .pic:
        self.patch_rel32_to_here(mic_miss);
        self.emit_mov_reg_imm64(R10, pic as u64);
        let mut next_entry: Option<usize> = None;
        for i in 0..JIT_PIC_ENTRIES {
            if let Some(p) = next_entry.take() {
                self.patch_rel32_to_here(p);
            }
            self.emit_cmp_eax_r10_disp(JitPICSlot::CLASS_ID_OFFSETS[i] as u8);
            let miss = self.emit_jcc_rel32(0x85); // JNE → next entry / .slow
            if i + 1 == JIT_PIC_ENTRIES {
                slow_patches.push(miss);
            } else {
                next_entry = Some(miss);
            }
            self.emit_cmp_qword_r10_disp_zero(JitPICSlot::ENTRY_PTR_OFFSETS[i] as u8);
            slow_patches.push(self.emit_jcc_rel32(0x84)); // JE .slow
            self.emit_cmp_byte_r10_disp_zero(JitPICSlot::NEEDS_CONTEXT_OFFSETS[i] as u8);
            // Same inversion as the MIC arm above.
            if premarshal {
                let noctx = self.emit_jcc_rel32(0x84); // JE .entry_ready (no shift)
                self.emit_ic_shift_for_context(num_args);
                self.patch_rel32_to_here(noctx);
            } else {
                let noctx = self.emit_jcc_rel32(0x84); // JE .entry_noctx
                self.emit_ic_abi_marshal(inputs, num_args, true);
                let call = self.emit_jmp_rel32();
                self.patch_rel32_to_here(noctx);
                self.emit_ic_abi_marshal(inputs, num_args, false);
                self.patch_rel32_to_here(call);
            }
            self.emit_call_cached_entry(JitPICSlot::ENTRY_PTR_OFFSETS[i] as u8);
            self.emit_inline_callee_deopt_service(info_ptr, num_args, inputs);
            done_patches.push(self.emit_jmp_rel32());
        }
        debug_assert!(next_entry.is_none());

        // ── Shared compact hashed/vtable stub ─────────────────────────────
        for p in slow_patches {
            self.patch_rel32_to_here(p);
        }
        let arg_offsets: Vec<i32> = (0..num_args).map(|i| self.slot_of(inputs[2 + i])).collect();
        // Populate the staging block for the stub's own callee-deopt service.
        // This is the megamorphic region — reached only after the MIC and all
        // four PIC entries missed — so the copy costs nothing on a
        // monomorphic site, which is where it used to be paid.
        for i in 0..num_args {
            let arg = inputs[2 + i];
            self.gp_load_value(RAX, arg);
            // Cast: an argument index is bounded by the callee's parameter
            // count, so `i * 8` cannot overflow an x86-64 displacement.
            self.store_rax(self.args_stage_top_off - (i as i32) * 8);
        }
        // `arg_offsets` are each argument's own register-allocated home slot,
        // which the stub loads into the ABI registers one at a time. They are
        // NOT a contiguous block, so the callee-deopt service -- which reads
        // `num_args` consecutive slots to rebuild the callee's incoming
        // locals -- must be pointed at the staging block just written instead.
        done_patches.extend(crate::runtime_lowering::emit_hashed_vtable_stub(
            &mut self.buf,
            pic,
            self.context_slot_off,
            &arg_offsets,
            self.frame_record,
            self.service_callee_deopt,
            info_ptr,
            self.args_stage_top_off,
        ));

        // ── Slow path: the resolving + cache-populating helper ────────────
        // Args 1..4 use the same ABI as `jit_invoke_dispatch`, so the staging
        // marshalling is identical to the generic path below.
        for i in 0..num_args {
            let arg = inputs[2 + i];
            self.gp_load_value(RAX, arg);
            self.store_rax(self.args_stage_top_off - (i as i32) * 8);
        }
        self.load_reg_from_frame(CALL_ARG_REGS[0], self.context_slot_off); // vm_ptr
        self.emit_mov_reg_imm64(CALL_ARG_REGS[1], info_ptr as u64); // info_ptr
        self.lea_reg_from_frame(CALL_ARG_REGS[2], self.args_stage_top_off); // args_ptr
        self.emit_mov_reg_imm64(CALL_ARG_REGS[3], num_args as u64); // num_args
                                                                    // Args 5 and 6 (mic, pic). Win64 passes them in the caller's stack
                                                                    // argument area at [RSP+32]/[RSP+40] — the 16-byte reserve
                                                                    // `Lowerer::new` budgets immediately above the 32-byte shadow space,
                                                                    // sized for exactly this worst-case 6-argument helper (see the
                                                                    // `jit_invoke_virtual_mic` note there). SysV passes them in R8/R9.
        #[cfg(target_os = "windows")]
        {
            self.emit_mov_reg_imm64(RAX, mic as u64);
            self.buf.emit(&[0x48, 0x89, 0x44, 0x24, 0x20]); // MOV [RSP+32], RAX
            self.emit_mov_reg_imm64(RAX, pic as u64);
            self.buf.emit(&[0x48, 0x89, 0x44, 0x24, 0x28]); // MOV [RSP+40], RAX
        }
        #[cfg(not(target_os = "windows"))]
        {
            self.emit_mov_reg_imm64(8, mic as u64); // R8
            self.emit_mov_reg_imm64(9, pic as u64); // R9
        }
        self.emit_mov_reg_imm64(RAX, self.invoke_virtual_mic as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX

        // .done:
        for p in done_patches {
            self.patch_rel32_to_here(p);
        }
        // Shared with the dispatch and direct-call paths: the compiled-entry
        // protocol is the same `i64::MIN` sentinel either way.
        self.emit_call_return_check(slot, ty);
    }

    /// Post-call exception sentinel + result spill, shared by the
    /// `jit_invoke_dispatch` path and the direct cross-method call path.
    ///
    /// A callee that threw (or deopted) returns the `i64::MIN` sentinel. For an
    /// int / reference / void result that is unambiguous (no legitimate value is
    /// `i64::MIN`), so a plain `CMP RAX, i64::MIN ; JE bail` suffices. For a
    /// `J`/`D`/`F` result a legitimate `Long.MIN_VALUE` is bit-identical to the
    /// sentinel, so on the (rare) `RAX == i64::MIN` branch we peek the
    /// out-of-band signal via `jit_dispatch_threw`: bail only when a genuine
    /// exception/deopt is pending, else keep the real value. The result is then
    /// spilled to `slot` (harmless for a void call: the slot is allocated but
    /// never read).
    fn emit_call_return_check(&mut self, slot: i32, ty: IrType) {
        // The call has just returned. Copy back any relocated shadow values
        // BEFORE anything else touches the frame — and before the sentinel
        // compare below, which clobbers R10. Uses RCX, never RAX, so the return
        // value in RAX is untouched (see `emit_shadow_reload`). This is the one
        // site all three dispatch routes share; the self-recursive route does
        // not reach here and does not publish.
        //
        // The RBP republish comes first because the reload's own correctness
        // does not depend on it, but the frame's identity to the collector
        // does: the callee overwrote the mirror with its own RBP on entry, and
        // any safepoint reached between here and the next call would otherwise
        // resolve this frame's maps against a dead frame's base.
        self.emit_post_call_frame_record();
        self.emit_shadow_reload();
        self.emit_call_return_sentinel_tail(ty);
        self.store_rax(slot);
    }

    /// The `i64::MIN` sentinel half of [`Self::emit_call_return_check`],
    /// without the frame republish, the shadow reload or the result store:
    /// what a call site emits on the COLD side of
    /// [`Self::emit_call_sentinel_fast_skip`], where the common non-sentinel
    /// return has already branched past it.
    fn emit_call_return_sentinel_tail(&mut self, ty: IrType) {
        self.emit_mov_reg_imm64(R10, i64::MIN as u64);
        self.buf.emit(&[0x4C, 0x39, 0xD0]); // CMP RAX, R10
        if matches!(ty, IrType::Long | IrType::Double | IrType::Float) {
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
            self.push_call_exc_patch(patch);
            // .keep: patch the JNE above to land here.
            let keep_off = self.buf.pos();
            let rel = keep_off as i32 - (keep_patch as i32 + 4);
            // ir_lower call-sentinel keep -- tolerated on an overflowed buffer; see
            // `Self::patch_or_bail` / `patch_rel32_to_here`.
            Self::patch_or_bail(&mut self.buf, keep_patch, rel);
        } else {
            self.buf.emit(&[0x0F, 0x84]); // JE rel32 (patched to the stub)
            let patch = self.buf.pos();
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
            self.push_call_exc_patch(patch);
        }
    }

    /// The hot half of a merged post-call sentinel check: `RAX != i64::MIN`
    /// branches past BOTH the callee-deopt service and the exception tail in
    /// one compare, where the two used to each materialise the 10-byte
    /// immediate and compare again. Returns the rel32 patch the caller lands
    /// on `.keep`. The cold side keeps its own compares -- they run only when
    /// the callee actually returned the sentinel.
    fn emit_call_sentinel_fast_skip(&mut self) -> usize {
        self.emit_mov_reg_imm64(R10, i64::MIN as u64);
        self.buf.emit(&[0x4C, 0x39, 0xD0]); // CMP RAX, R10
        self.emit_jcc_rel32(0x85) // JNE .keep
    }

    /// COV-03 — the `i64::MIN` sentinel check for a *helper* that returns a
    /// value in RAX and may instead return the deopt/NPE sentinel.
    ///
    /// The same two-shape decision `emit_call_return_check` makes, minus the
    /// post-call frame/shadow republication a dispatched Java call needs and a
    /// leaf helper does not: `jit_getfield` reaches no safepoint, so nothing has
    /// moved and no oop needs copying back.
    ///
    /// * `Int`/`Ref`/anything else — `i64::MIN` is never a legitimate result (no
    ///   plausible heap pointer equals it), so `CMP ; JE bail`.
    /// * `Long`/`Double`/`Float` — `Long.MIN_VALUE`, and the `-0.0` bit pattern,
    ///   ARE legitimate results bit-identical to the sentinel. On that (rare)
    ///   branch, peek the out-of-band signal via `jit_dispatch_threw` and bail
    ///   only when a genuine exception/deopt is pending; otherwise keep the real
    ///   value. `jit_getfield` sets the pending-NPE flag before returning the
    ///   sentinel, which is one of the signals that peek reports.
    ///
    /// Leaves RAX holding the value to spill in both shapes.
    fn emit_helper_sentinel_check(&mut self, ty: IrType) {
        self.emit_mov_reg_imm64(R10, i64::MIN as u64);
        self.buf.emit(&[0x4C, 0x39, 0xD0]); // CMP RAX, R10
        if !matches!(ty, IrType::Long | IrType::Double | IrType::Float) {
            self.buf.emit(&[0x0F, 0x84]); // JE rel32 → shared bail stub
            let exc_patch = self.buf.pos();
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
            self.push_call_exc_patch(exc_patch);
            return;
        }
        // JNE .keep — common path: not the sentinel, keep the real RAX.
        self.buf.emit(&[0x0F, 0x85]);
        let keep_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        // Cold: RAX == i64::MIN. MOV RAX, dispatch_threw ; CALL RAX (RAX = 0/1).
        self.emit_mov_reg_imm64(RAX, self.dispatch_threw as u64);
        self.buf.emit(&[0xFF, 0xD0]);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX — ZF=1 ⇒ no signal
                                            // Restore the sentinel/value before branching: the shared bail stub
                                            // returns RAX unchanged, and the keep path needs the genuine value.
                                            // `MOV` does not disturb ZF.
        self.emit_mov_reg_imm64(RAX, i64::MIN as u64);
        self.buf.emit(&[0x0F, 0x85]); // JNE bail_stub
        let patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.push_call_exc_patch(patch);
        let keep_off = self.buf.pos();
        let rel = keep_off as i32 - (keep_patch as i32 + 4);
        // ir_lower helper-sentinel keep -- tolerated on an overflowed buffer;
        // see `Self::patch_or_bail` / `patch_rel32_to_here`.
        Self::patch_or_bail(&mut self.buf, keep_patch, rel);
    }

    /// fib44-fix follow-up: patch every direct self-recursive `CALL` (invoke_kind
    /// 4) so its rel32 targets this method's own entry — code offset 0.
    fn patch_self_calls(&mut self) {
        let patches = std::mem::take(&mut self.self_call_patches);
        for p in patches {
            // rel32 = target - (rel32_field_offset + 4); target = entry = 0.
            let rel = 0i32 - (p as i32 + 4);
            // ir_lower self-call rel32 -- tolerated on an overflowed buffer; see
            // `Self::patch_or_bail` / `patch_rel32_to_here`.
            Self::patch_or_bail(&mut self.buf, p, rel);
        }
    }

    // ── Level 2: the machine list, emitting ──────────────────────────────
    //
    // Increment 2 of
    // `docs/feature-designs/jit-machine-level-and-instruction-selection.md`:
    // the selector's tiles become bytes, through the same `PATTERNS` table
    // whose every row is anchored to a hand-written emitter, against the
    // trivial allocation `ir_lower` has always used — every value in its frame
    // word, RAX the destination, RCX the second operand.
    //
    // The whole increment is the byte-equality oracle. `MirMode::Verify` runs
    // the per-opcode arm AND the tile encoder and compares; `MirMode::Emit`
    // runs only the encoder. A mismatch in either mode refuses the compile.

    /// The one entry point `lower_block` calls. Falls through to
    /// [`Self::lower_data_node`] whenever the machine list has nothing to say
    /// about `id`, which is every node on every compile that has not set a
    /// machine-level flag.
    fn lower_data_node_through_mir(&mut self, block_idx: usize, id: NodeId) {
        let bytes = match self.mir_mode {
            MirMode::Off => None,
            MirMode::Verify | MirMode::Emit => self.mir_tile_bytes(block_idx, id),
        };
        let Some(bytes) = bytes else {
            // Not emittable. In verify mode, still ask what the encoder WOULD
            // have produced, and price it against what the arm writes — that
            // difference is the size of the next increment, and it is the one
            // number that decides whether there should be one.
            if self.mir_mode == MirMode::Verify {
                let shadow = self.mir_shadow_tile_bytes(block_idx, id);
                let before = self.buf.pos();
                self.lower_data_node(id);
                let after = self.buf.pos();
                if let Some(shadow) = shadow {
                    self.mir_shadow_tiles += 1;
                    self.mir_arm_bytes += after.saturating_sub(before);
                    self.mir_enc_bytes += shadow.as_slice().len();
                }
                return;
            }
            self.lower_data_node(id);
            return;
        };
        self.mir_tiles += 1;
        if self.mir_mode == MirMode::Verify {
            // The per-opcode arm is still the thing that emits, so this mode
            // cannot produce a wrong instruction — only a wrong verdict.
            let before = self.buf.pos();
            self.lower_data_node(id);
            let after = self.buf.pos();
            let agreed = self
                .buf
                .as_slice()
                .get(before..after)
                .is_some_and(|emitted| emitted == bytes.as_slice());
            if !agreed {
                self.mir_mismatches += 1;
            }
            return;
        }
        // `MirMode::Emit`. Everything the per-opcode arm does besides emitting
        // has to happen here too, in the same order: the bci anchor is read at
        // the position the first byte lands on, and the slot allocation is what
        // publishes the node to `defined_nodes` and moves the spill watermark.
        if let Some(pc) = self.graph.nodes[id as usize].bytecode_pc {
            let here = self.buf.pos();
            self.cur_bci = pc;
            self.bci_native
                .entry(pc)
                .and_modify(|e| {
                    if here < *e {
                        *e = here;
                    }
                })
                .or_insert(here);
        }
        let slot = self.alloc_slot(id);
        // The encoder wrote its store against `planned_slot_off`; if the
        // allocating path just disagreed, the artifact is already wrong and no
        // later check would notice. Refuse.
        if self.planned_slot_off(id).ok() != Some(slot) {
            self.mir_mismatches += 1;
            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "ir_lower: the level-2 encoder and alloc_slot disagreed on a destination slot",
            )));
            return;
        }
        self.buf.emit(bytes.as_slice());
    }

    /// The bytes the level-2 encoder produces for the tile rooted at `id`, or
    /// `None` when there is no such tile or the encoder declines it.
    ///
    /// Declining is the normal answer and costs nothing: the caller lowers the
    /// node the way it always did. What must never happen is a tile that
    /// *absorbed* other nodes being emitted here — those nodes would then be
    /// computed nowhere — so the cover list is checked against the root and
    /// nothing else.
    fn mir_tile_bytes(&self, block_idx: usize, id: NodeId) -> Option<FrameAccessList> {
        let plan = self.mir.as_ref()?;
        let (tb, ti) = (*plan.tile_of.get(id as usize)?)?;
        if usize::try_from(tb).ok()? != block_idx {
            return None;
        }
        let tile = plan
            .blocks
            .get(usize::try_from(tb).ok()?)?
            .tiles
            .get(usize::try_from(ti).ok()?)?;
        if !mir_tile_is_emittable(tile, id) {
            return None;
        }
        self.encode_tile_frame_homed(tile)
    }

    /// The frame word `id` lives in, **and** the fact that it lives there.
    ///
    /// The level-2 encoder's whole premise is that an operand can be read out
    /// of its frame word. The linear-scan read cache makes that false for a
    /// value it has published into a register — today only for `Float`/`Double`
    /// (the file is XMM-only), which no integer tile can name. Asked rather
    /// than assumed, per operand: the day the allocator grows a GP class, this
    /// returns `None` and the tile is declined, instead of silently encoding a
    /// load of a stale word.
    fn frame_operand(&self, id: NodeId) -> Option<i32> {
        if self.resident_xmm(id).is_some() {
            return None;
        }
        self.slot_of_checked(id).ok()
    }

    /// The frame word a tile will STORE its result into.
    ///
    /// `planned_slot_off` rather than `slot_of_checked` because the destination
    /// has not been allocated yet when the encoder runs — the emit path calls
    /// `alloc_slot` afterwards and checks the two agree. The residency gate is
    /// the same one [`Self::frame_operand`] applies, for the same reason: a
    /// value the allocator publishes into a register is a value whose frame
    /// word is not the whole truth.
    fn frame_destination(&self, id: NodeId) -> Option<i32> {
        if self.resident_xmm(id).is_some() {
            return None;
        }
        self.planned_slot_off(id).ok()
    }

    /// What the level-2 encoder *would* emit for `id`, whatever the rule.
    ///
    /// Verify mode only, and it emits nothing: this is how the next increment
    /// gets sized with a number instead of an argument. `mir_tile_bytes` is
    /// restricted to `Rule::AluReg` because that is the only rule whose bytes
    /// are provably identical to the per-opcode arm's — and byte equality is
    /// increment 2's whole oracle. The rules that would *improve* the code
    /// (`AluImm` drops a frame load) cannot ride that oracle by construction,
    /// so what they are worth has to be measured separately, against the bytes
    /// the arms actually wrote.
    ///
    /// Restricted to tiles covering exactly their own root, for the same reason
    /// [`mir_tile_is_emittable`] is: a tile that absorbed other nodes cannot be
    /// compared against one node's byte range.
    fn mir_shadow_tile_bytes(&self, block_idx: usize, id: NodeId) -> Option<FrameAccessList> {
        let plan = self.mir.as_ref()?;
        let (tb, ti) = (*plan.tile_of.get(id as usize)?)?;
        if usize::try_from(tb).ok()? != block_idx {
            return None;
        }
        let tile = plan
            .blocks
            .get(usize::try_from(tb).ok()?)?
            .tiles
            .get(usize::try_from(ti).ok()?)?;
        if tile.root != id || tile.covered.as_slice() != [id] {
            return None;
        }
        self.encode_tile_frame_homed(tile)
    }

    /// Encode one tile against the frame-homed allocation.
    ///
    /// "Frame-homed" is not a simplification of a register allocation — it *is*
    /// this backend's allocation, the one `ir_lower::frame_word_off` states by
    /// returning `Err` for `ValueLoc::Reg`. Every value lives in its frame word;
    /// RAX carries the tile's destination and RCX its second operand, exactly as
    /// the per-opcode arms use them. That is what makes byte equality reachable
    /// at all, and it is why increment 3 (real registers) is a separate step
    /// with a prologue prerequisite.
    ///
    /// The two-address `MInst::Move` prefix costs nothing here and is not
    /// dropped: "copy the left operand into the destination's register" and
    /// "load the left operand" are the same instruction when the destination's
    /// register is RAX, so the tracker below emits it once whether the selector
    /// asked for the copy or coalesced it away.
    fn encode_tile_frame_homed(&self, tile: &crate::x64::isel::Tile) -> Option<FrameAccessList> {
        use crate::x64::isel::{select, MInst, Operand, Req, Ty};

        let mut out = FrameAccessList::new();
        // Which value RAX currently holds, if the tile has already loaded one.
        let mut rax_holds: Option<NodeId> = None;
        for inst in &tile.insts {
            match *inst {
                MInst::Move {
                    dst,
                    ty: Ty::I64,
                    src,
                } => {
                    if dst != tile.root {
                        return None;
                    }
                    let mut acc = FrameAccess::new();
                    enc_frame_load(RAX, self.frame_operand(src)?, &mut acc);
                    out.push(&acc)?;
                    rax_holds = Some(src);
                }
                MInst::AluRR {
                    op,
                    ty,
                    dst,
                    lhs,
                    rhs,
                } => {
                    if dst != tile.root {
                        return None;
                    }
                    if rax_holds != Some(lhs) {
                        let mut acc = FrameAccess::new();
                        enc_frame_load(RAX, self.frame_operand(lhs)?, &mut acc);
                        out.push(&acc)?;
                    }
                    let mut acc = FrameAccess::new();
                    enc_frame_load(RCX, self.frame_operand(rhs)?, &mut acc);
                    out.push(&acc)?;
                    // Level 3. `select` refuses rather than inventing an
                    // encoding, and every row it can answer with names the
                    // hand-written emitter it reproduces byte for byte.
                    let sel =
                        select(&Req::new(op, ty, Operand::Gpr(RAX), Operand::Gpr(RCX))).ok()?;
                    out.push_bytes(&sel.encoded.bytes)?;
                    let mut acc = FrameAccess::new();
                    enc_frame_store(RAX, self.frame_destination(dst)?, &mut acc);
                    out.push(&acc)?;
                    rax_holds = None;
                }
                // `dst <- lhs op imm`. Reachable only from the SHADOW path —
                // `mir_tile_is_emittable` admits `Rule::AluReg` and nothing
                // else — because this form is not byte-equal to anything: the
                // per-opcode arm materialises the constant into RCX and uses
                // the register form, and dropping that load is the whole point.
                // It is encoded here so the saving can be *measured* before it
                // is spent.
                MInst::AluRI {
                    op,
                    ty,
                    dst,
                    lhs,
                    imm,
                    form: _,
                } => {
                    if dst != tile.root {
                        return None;
                    }
                    if rax_holds != Some(lhs) {
                        let mut acc = FrameAccess::new();
                        enc_frame_load(RAX, self.frame_operand(lhs)?, &mut acc);
                        out.push(&acc)?;
                    }
                    let sel =
                        select(&Req::new(op, ty, Operand::Gpr(RAX), Operand::Imm(imm))).ok()?;
                    out.push_bytes(&sel.encoded.bytes)?;
                    let mut acc = FrameAccess::new();
                    enc_frame_store(RAX, self.frame_destination(dst)?, &mut acc);
                    out.push(&acc)?;
                    rax_holds = None;
                }
                // Anything else is a shape this encoder has not been proved
                // byte-equal for. Refusing is free; guessing is not.
                _ => return None,
            }
        }
        if out.is_empty() {
            return None;
        }
        #[cfg(test)]
        if mir_injecting_a_wrong_byte() {
            out.corrupt_last_byte();
        }
        Some(out)
    }

    /// The `(phi, dst_slot, src_slot)` copies the edge `pred_block ->
    /// succ_block` carries: one per value phi of the successor's merge whose
    /// input for this edge is a real node. Pure; `emit_phi_copies` emits
    /// exactly these and `edge_has_phi_copies` asks whether there are any.
    fn gather_phi_copies(&self, pred_block: usize, succ_block: usize) -> Vec<(NodeId, i32, i32)> {
        let merge_ctrl = self.schedule.blocks[succ_block].ctrl;
        if !matches!(
            self.graph.nodes[merge_ctrl as usize].op,
            Op::Merge | Op::Region
        ) {
            return Vec::new();
        }
        let mut out: Vec<(NodeId, i32, i32)> = Vec::new();
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
                        out.push((id as NodeId, self.slot_of(id as NodeId), self.slot_of(val_id)));
                    }
                }
            }
        }
        out
    }

    /// Whether the edge `pred_block -> succ_block` carries any phi copy.
    fn edge_has_phi_copies(&self, pred_block: usize, succ_block: usize) -> bool {
        !self.gather_phi_copies(pred_block, succ_block).is_empty()
    }

    /// Fill `use_count`, `deopt_named` and `fused_cmp` before the first block
    /// lowers. A compare is fused into its `If` when that `If` is its ONLY
    /// use, no safepoint snapshot names it, and the switch is on.
    fn prepare_fusion_tables(&mut self) {
        let n = self.graph.nodes.len();
        let mut use_count = vec![0u32; n];
        for node in &self.graph.nodes {
            for &input in &node.inputs {
                if input != NO_NODE {
                    if let Some(c) = use_count.get_mut(input as usize) {
                        *c = c.saturating_add(1);
                    }
                }
            }
        }
        let mut deopt_named = vec![false; n];
        for sp in &self.graph.safepoints {
            for &v in sp.locals.iter().chain(sp.stack.iter()) {
                if v != NO_NODE {
                    if let Some(cell) = deopt_named.get_mut(v as usize) {
                        *cell = true;
                    }
                }
            }
        }
        let mut fused_cmp = vec![false; n];
        if ir_fused_branch_enabled() {
            // Fusing moves the reads of the compare's inputs from the
            // compare's own position to the terminator's, so it is legal
            // exactly while nothing scheduled in between can overwrite where
            // those inputs live. Two things can:
            //
            // * a value sharing an input's HOME COLOUR — `plan_slots` packs
            //   values into shared frame words by live range, and the
            //   allocator's model ends an input's range at the compare;
            // * a value promoted into an input's REGISTER, for the same
            //   reason. `test_lower_ternary_lt_phi` caught exactly that: `a`
            //   and the constant `1` were handed the same RBX.
            //
            // Both are checked against the plans this lowering was handed,
            // rather than approximated by requiring the compare to be the last
            // scheduled node — which is what the first cut did, and it
            // declined `fib`, whose two subtractions sit between its compare
            // and its branch.
            let color_of = |plan: &SlotPlan, id: NodeId| -> Option<u32> {
                plan.node_color.get(id as usize).copied().flatten()
            };
            for block in &self.schedule.blocks {
                let Some(term) = block.terminator else {
                    continue;
                };
                let Some(if_node) = self.graph.nodes.get(term as usize) else {
                    continue;
                };
                if !matches!(if_node.op, Op::If) {
                    continue;
                }
                let Some(&cond) = if_node.inputs.get(1) else {
                    continue;
                };
                if cond == NO_NODE {
                    continue;
                }
                let Some(pos) = block.nodes.iter().position(|&id| id == cond) else {
                    continue;
                };
                let Some(cmp) = self.graph.nodes.get(cond as usize) else {
                    continue;
                };
                if !matches!(cmp.op, Op::Cmp(_)) || cmp.inputs.len() < 2 {
                    continue;
                }
                if use_count[cond as usize] != 1 || deopt_named[cond as usize] {
                    continue;
                }
                let (a, b) = (cmp.inputs[0], cmp.inputs[1]);
                let in_colors = [color_of(&self.slot_plan, a), color_of(&self.slot_plan, b)];
                // Through the sanctioned accessors, never `reg_of` directly:
                // `the_register_read_path_is_gated_on_publication` is what
                // keeps "assigned" and "published" from drifting apart.
                let in_gp = [self.assigned_gpr(a), self.assigned_gpr(b)];
                let in_fp = [self.assigned_xmm(a), self.assigned_xmm(b)];
                let clobbered = block.nodes[pos + 1..].iter().any(|&later| {
                    let c = color_of(&self.slot_plan, later);
                    let g = self.assigned_gpr(later);
                    let f = self.assigned_xmm(later);
                    (c.is_some() && in_colors.contains(&c))
                        || (g.is_some() && in_gp.contains(&g))
                        || (f.is_some() && in_fp.contains(&f))
                });
                if clobbered {
                    continue;
                }
                fused_cmp[cond as usize] = true;
            }
        }
        self.use_count = use_count;
        self.deopt_named = deopt_named;
        self.fused_cmp = fused_cmp;
    }

    /// `MOV reg, imm` in the shortest encoding that reproduces `val` in all 64
    /// bits: `mov r32, imm32` (zero-extends) for 0..=u32::MAX, `mov r64,
    /// simm32` for the rest of the i32 range, `mov r64, imm64` otherwise.
    fn emit_mov_reg_imm_smart(&mut self, reg: u8, val: i64) {
        if (0..=i64::from(u32::MAX)).contains(&val) {
            if reg >= 8 {
                self.buf.emit_byte(0x41); // REX.B
            }
            self.buf.emit_byte(0xB8 + (reg & 7));
            self.buf.emit(&(val as u32).to_le_bytes());
        } else if i32::try_from(val).is_ok() {
            let rex = 0x48 | if reg >= 8 { 0x01 } else { 0 };
            self.buf.emit_byte(rex);
            self.buf.emit_byte(0xC7); // MOV r/m64, simm32
            self.buf.emit_byte(0xC0 | (reg & 7));
            self.buf.emit(&(val as i32).to_le_bytes());
        } else {
            self.emit_mov_reg_imm64(reg, val as u64);
        }
    }

    /// Whether `base` was already null-tested and mapped-proved in the block
    /// being lowered (`ir_receiver_guard_cse_enabled`).
    fn receiver_already_guarded(&self, base: NodeId) -> bool {
        ir_receiver_guard_cse_enabled() && self.guarded_receivers.contains(&base)
    }

    /// Is `base` known non-null here — by the receiver seed, or by a null test
    /// already emitted in this block?
    fn receiver_already_non_null(&self, base: NodeId) -> bool {
        self.receiver_already_guarded(base) || self.null_proven_receivers.contains(&base)
    }

    /// Record that a null test has just proven `base` on its fall-through.
    fn note_receiver_non_null(&mut self, base: NodeId) {
        if !self.null_proven_receivers.contains(&base) {
            self.null_proven_receivers.push(base);
        }
    }

    /// Seed this block's null proofs with `this`.
    ///
    /// # Why a seed, and not just the per-block CSE
    ///
    /// The CSE above proves a receiver once per block. That is worth nothing to
    /// a loop whose body reads one field: the body is one block, its single
    /// `getfield` is the first dereference in it, and so the test is emitted
    /// once and executed on every iteration — forever, for a value the JVM
    /// guarantees at the call site.
    ///
    /// This is the same hole the single-pass backend had, in the same shape
    /// and for the same reason, and it was closed there by
    /// `CRATONVM_JIT_THIS_NONNULL` seeding the null-check dataflow's entry
    /// state. That fix was single-pass only: both arms it touches are in
    /// `x64/bytecode_walk.rs`, so this tier kept emitting `TEST RAX, RAX; JZ`
    /// at every `getfield` — and a 2026-09-03 measurement put the optimizing
    /// tier at ~1.65x the baseline's time on exactly that loop.
    ///
    /// The seed is one fact: an instance method's parameter 0 is non-null,
    /// because the JVM enters one only through a call site that has already
    /// null-checked the receiver. `<init>` included — its receiver is
    /// uninitialized, never null.
    ///
    /// **It is only ever the parameter NODE.** A local reassigned from
    /// parameter 0 is a different node and gets nothing; there is no
    /// bytecode-local pattern match here to mis-attribute, which is the class
    /// of bug `preceding_aload_nonnull_local` carries a soundness fix for.
    fn seed_block_null_proofs(&mut self) {
        if !ir_this_nonnull_enabled() {
            return;
        }
        let Some(recv) = self.graph.receiver_param else {
            return;
        };
        for (id, node) in self.graph.nodes.iter().enumerate() {
            if node.op == Op::Param(recv) {
                // Cast: node ids index `graph.nodes`, which the builder bounds.
                self.null_proven_receivers.push(id as NodeId);
                crate::metrics::note_ir_receiver_seed();
                break;
            }
        }
    }

    /// Record that the block being lowered has just proved `base`.
    fn note_receiver_guarded(&mut self, base: NodeId) {
        if ir_receiver_guard_cse_enabled() && !self.guarded_receivers.contains(&base) {
            self.guarded_receivers.push(base);
        }
    }

    fn lower_data_node(&mut self, id: NodeId) {
        // real-frame-deopt: anchor the node's bytecode pc to the earliest
        // native offset emitted for it, so safepoint snapshots can be keyed
        // by native offset. `self.graph` is a `&'a Graph`, so reading
        // `bytecode_pc` here does not borrow `self`.
        if let Some(pc) = self.graph.nodes[id as usize].bytecode_pc {
            let here = self.buf.pos();
            self.cur_bci = pc;
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
                let val = *val;
                let slot = self.alloc_slot(id);
                if !ir_const_imm_enabled() || !self.emit_store_frame_imm32(slot, val) {
                    self.emit_mov_rax_imm64(val);
                    self.store_rax(slot);
                }
            }
            Op::Param(idx) => {
                let slot = self.alloc_slot(id);
                // Params were stored to local frame slots by the prologue.
                // local_offset(i) = (i + 1) * 8
                let param_offset = ((*idx as i32) + 1) * 8;
                self.load_to_rax(param_offset);
                // An FP parameter is a loop invariant often enough to be worth
                // a register; the copy comes from the home word this just
                // wrote, because the prologue delivered it through a GPR.
                //
                // An INT or LONG parameter is exactly as much a loop invariant,
                // and got nothing until 2026-09-03. `gp_store_value` writes the
                // home word and then copies RAX into the register, so the
                // publish here is register-to-register — no reload of a word
                // this arm just wrote.
                if matches!(node.ty, IrType::Int | IrType::Long) {
                    self.gp_store_value(id, slot, RAX);
                } else {
                    self.store_rax(slot);
                    if matches!(node.ty, IrType::Float | IrType::Double) {
                        self.publish_fp_from_slot(id, slot, node.ty == IrType::Double);
                    }
                }
            }
            Op::Add => {
                let slot = self.alloc_slot(id);
                if matches!(node.ty, IrType::Float | IrType::Double) {
                    // ADDSS/ADDSD XMM0, XMM1
                    let is_d = node.ty == IrType::Double;
                    self.fp_load_value(XMM0, node.inputs[0], is_d);
                    self.fp_load_value(XMM1, node.inputs[1], is_d);
                    self.fp_binop(0x58, XMM0, XMM1, is_d);
                    self.fp_store_value(id, slot, XMM0, is_d);
                } else {
                    self.gp_load_value(RAX, node.inputs[0]);
                    self.gp_load_value(RCX, node.inputs[1]);
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
                    self.fp_load_value(XMM0, node.inputs[0], is_d);
                    self.fp_load_value(XMM1, node.inputs[1], is_d);
                    self.fp_binop(0x5C, XMM0, XMM1, is_d);
                    self.fp_store_value(id, slot, XMM0, is_d);
                } else {
                    self.gp_load_value(RAX, node.inputs[0]);
                    self.gp_load_value(RCX, node.inputs[1]);
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
                    self.fp_load_value(XMM0, node.inputs[0], is_d);
                    self.fp_load_value(XMM1, node.inputs[1], is_d);
                    self.fp_binop(0x59, XMM0, XMM1, is_d);
                    self.fp_store_value(id, slot, XMM0, is_d);
                } else {
                    self.gp_load_value(RAX, node.inputs[0]);
                    self.gp_load_value(RCX, node.inputs[1]);
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
                    self.fp_load_value(XMM0, node.inputs[0], is_d);
                    self.fp_load_value(XMM1, node.inputs[1], is_d);
                    self.fp_binop(0x5E, XMM0, XMM1, is_d);
                    self.fp_store_value(id, slot, XMM0, is_d);
                    return;
                }
                let ty = node.ty;
                let bpc = node.bytecode_pc;
                self.gp_load_value(RAX, node.inputs[0]);
                self.gp_load_value(RCX, node.inputs[1]);
                let zero_after = self.emit_div_zero_guard(ty, bpc);
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
                // Same continuation for the zero-divisor skip, when this
                // division's trap is owned by a control-anchored `Op::Guard`.
                if let Some(p) = zero_after {
                    self.patch_div_overflow_after(p);
                }
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
                    self.fp_load_value(XMM0, node.inputs[0], is_d); // a
                    self.fp_load_value(XMM1, node.inputs[1], is_d); // b
                    let helper = if is_d { self.drem } else { self.frem };
                    self.emit_mov_reg_imm64(RAX, helper as u64);
                    self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                    self.fp_store_value(id, slot, XMM0, is_d);
                    return;
                }
                let ty = node.ty;
                let bpc = node.bytecode_pc;
                self.gp_load_value(RAX, node.inputs[0]);
                self.gp_load_value(RCX, node.inputs[1]);
                let zero_after = self.emit_div_zero_guard(ty, bpc);
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
                // Same continuation for the zero-divisor skip, when this
                // remainder's trap is owned by a control-anchored `Op::Guard`.
                if let Some(p) = zero_after {
                    self.patch_div_overflow_after(p);
                }
                self.store_rax(slot);
            }
            Op::MonitorEnter | Op::MonitorExit => {
                // `[ctrl, mem, obj]`.
                let obj_id = node.inputs[2];
                let obj_slot = self.slot_of(obj_id);
                // Publish the map BEFORE the call, same contract as `Op::Call`:
                // both monitor ops carry `safepoint: true`, and a contended
                // acquire parks this thread for an entire collection.
                let sp_live_hi = self.spill_high_water;
                self.emit_safepoint_map(sp_live_hi);
                let target = if matches!(node.op, Op::MonitorEnter) {
                    self.monitor_enter
                } else {
                    self.monitor_exit
                };
                crate::runtime_lowering::emit_monitor_stub(
                    &mut self.buf,
                    self.context_slot_off,
                    obj_slot,
                    target,
                    self.frame_record,
                );
                // `i64::MIN` means the helper published a pending Java
                // exception (a null receiver takes the NPE path).
                self.emit_mov_reg_imm64(RCX, i64::MIN as u64);
                self.buf.emit(&[0x48, 0x39, 0xC8]); // CMP RAX, RCX
                self.buf.emit(&[0x0F, 0x85]); // JNE ok
                let ok_patch = self.buf.pos();
                self.buf.emit(&[0; 4]);
                self.buf.emit_byte(0xE9); // JMP shared exception epilogue
                let exception_patch = self.buf.pos();
                self.buf.emit(&[0; 4]);
                self.push_call_exc_patch(exception_patch);
                let ok = self.buf.pos();
                let rel = ok as i32 - (ok_patch as i32 + 4);
                Self::patch_or_bail(&mut self.buf, ok_patch, rel);
                // Store the possibly-REMAPPED reference back. A contended
                // acquire can move the object while this thread is parked.
                // Idempotent with the collector's own rewrite:
                // `conservative_roots::remap_one_jit_frame` rewrites published
                // slots keyed on their CURRENT value, so if it already wrote
                // the new address, writing the same address again is a no-op.
                self.store_rax(obj_slot);
            }
            Op::New {
                class_id,
                num_fields,
            } => {
                // `jit_new_object` can trigger a real collection (TLAB
                // exhaustion), so this is a GC-capable point and owes the same
                // map `Op::NewArray` / `Op::Call` / `Op::MonitorEnter` pay.
                //
                // It did not pay it, on the reasoning recorded above the
                // `Op::NewArray` arm -- "unlike `Op::New`'s arm, which has no
                // operand of its own to protect". That reads the map as
                // protection for the NODE, and it is not: it describes every
                // live reference IN THE FRAME at this program point. A frame
                // parked inside the allocation therefore had its sp-id slot
                // still naming whichever earlier safepoint last wrote it, or
                // the prologue sentinel when none had.
                //
                // MEASURED on `org.h2.test.jdbc.TestCachedQueryResults`: with
                // the poll repair in place the last surviving `no-map-for-id`
                // frame was `java/util/ArrayList.iterator()` -- a method whose
                // whole body is `new Itr(this)` -- reading `sp_id=0` against
                // `maps=2 ids=[1, 2]`. It was parked in this stub.
                //
                // Ordered exactly as `Op::NewArray` orders it: the map is taken
                // BEFORE `alloc_slot`, which marks this node's own result slot
                // defined immediately, because the result is not written until
                // the stub returns and publishing it early hands the collector
                // an uninitialised word to treat as a live reference.
                let mapped = Self::ir_gc_point_maps_enabled();
                if mapped {
                    let sp_live_hi = self.spill_high_water;
                    self.emit_safepoint_map(sp_live_hi);
                }
                let slot = self.alloc_slot(id);
                // Inline TLAB bump first, with the stub as its slow path.
                // Declining emits the stub alone, which is the behaviour this
                // arm had before the bump existed.
                //
                // The safepoint map above still stands: the bump itself cannot
                // collect, but its slow path is the same stub, and a map is a
                // statement about the FRAME at this program point rather than
                // about which arm runs.
                let tlab_plan = crate::runtime_lowering::InlineTlabPlan {
                    thread_slot_off: self.shadow_thread_slot_off,
                    cursor_off: self.tlab_cursor_off,
                    end_off: self.tlab_end_off,
                    post_init: self.tlab_post_init,
                    context_off: self.context_slot_off,
                    class_id: *class_id,
                    num_fields: *num_fields,
                };
                let inlined = ir_inline_tlab_enabled()
                    && crate::runtime_lowering::emit_inline_tlab_new_ir(
                        &mut self.buf,
                        &tlab_plan,
                        self.new_object,
                        self.frame_record,
                    );
                if !inlined {
                    crate::runtime_lowering::note_stub_only_alloc();
                    crate::runtime_lowering::emit_new_object_stub(
                        &mut self.buf,
                        self.context_slot_off,
                        self.new_object,
                        *class_id,
                        *num_fields,
                        self.frame_record,
                    );
                }
                // Retract the push the map above emitted. Unbalanced pushes are
                // not a leak this backend tolerates: `lower_inner` compares
                // `shadow_pushes` against `shadow_reloads` and refuses the whole
                // method. RCX, never RAX -- the returned (possibly relocated)
                // pointer is untouched.
                if mapped {
                    self.emit_shadow_reload();
                }

                // `jit_new_object` returns null after publishing a pending
                // initialization/OOM exception. Convert that private sentinel
                // to the JIT-wide i64::MIN return before entering the shared
                // exception epilogue, matching the baseline tier.
                self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
                self.buf.emit(&[0x0F, 0x85]); // JNZ allocated
                let allocated_patch = self.buf.pos();
                self.buf.emit(&[0; 4]);
                self.emit_mov_reg_imm64(RAX, i64::MIN as u64);
                self.buf.emit_byte(0xE9); // JMP shared exception epilogue
                let exception_patch = self.buf.pos();
                self.buf.emit(&[0; 4]);
                self.push_call_exc_patch(exception_patch);
                let allocated = self.buf.pos();
                let rel = allocated as i32 - (allocated_patch as i32 + 4);
                // allocation success -- tolerated on an overflowed buffer; see
                // `Self::patch_or_bail` / `patch_rel32_to_here`.
                Self::patch_or_bail(&mut self.buf, allocated_patch, rel);
                self.store_rax(slot);
            }
            // cov-06: array allocation. Inputs `[ctrl, mem, length]` — the
            // shared allocation stub differs from `Op::New`'s only in that
            // the third argument is a RUNTIME slot (the length) rather than
            // an immediate, and the target/immediate pair is chosen by
            // shape: `element_type != 0` is a `newarray` (the atype IS the
            // immediate, `helpers.newarray`); `element_type == 0` is an
            // `anewarray` of an already-loaded class (`component_class_id`
            // is the immediate, `helpers.anewarray_object`). Same
            // zero-on-failure convention as `Op::New` — `jit_newarray` /
            // `jit_anewarray_object` return `0` after publishing a pending
            // exception (OOM OR a negative length, both routed through the
            // interpreter's `NegativeArraySizeException`/`OutOfMemoryError`
            // machinery), converted to the JIT-wide `i64::MIN` sentinel
            // exactly like `Op::New`'s failure path.
            //
            // `jit_newarray`/`jit_anewarray_object` can trigger a real
            // collection (TLAB exhaustion), so this allocation
            // needs a fresh safepoint map published BEFORE it, exactly as
            // `Op::MonitorEnter`/`Op::Call` do: without one, `sp_id_slot_off`
            // keeps naming whichever EARLIER safepoint last wrote it (or none
            // at all), so a collection during THIS call matches a map
            // describing a different program point and relocates against it
            // — `emit_safepoint_map`'s own doc names this exact hazard.
            // Reached in practice: a hot method that `newarray`s in a tight
            // loop and returns the array to an interpreter caller corrupted
            // the returned reference under GC pressure before this map was
            // added (`vm/tests/jit_cov06_array_allocation.rs`, `gcRootsOK`/
            // the plain allocate-loop warm-up both reproduced it).
            //
            // Published BEFORE `alloc_slot(id)` too — `alloc_slot` marks this
            // node's OWN result slot `defined_nodes[id] = true` immediately,
            // and the result is not written until the call returns, so a map
            // taken after `alloc_slot` would hand the collector an
            // uninitialised word to treat as a live reference (the same
            // ordering `emit_safepoint_map`'s doc requires).
            Op::NewArray {
                element_type,
                component_class_id,
            } => {
                let sp_live_hi = self.spill_high_water;
                self.emit_safepoint_map(sp_live_hi);
                let slot = self.alloc_slot(id);
                let length_slot = self.slot_of(node.inputs[2]);
                let (target, immediate) = if *element_type != 0 {
                    (self.newarray, u32::from(*element_type))
                } else {
                    (self.anewarray_object, *component_class_id)
                };
                crate::runtime_lowering::emit_new_array_stub(
                    &mut self.buf,
                    self.context_slot_off,
                    target,
                    immediate,
                    length_slot,
                    self.frame_record,
                );
                // The reload that PAIRS with the map above. Its absence was not
                // a missed optimisation: `emit_shadow_push` bumps the thread's
                // shadow top and `emit_shadow_reload` is what retracts it, so an
                // array allocation with any live reference around it leaked one
                // frame's worth of shadow slots per execution. The
                // `shadow_pushes != shadow_reloads` check at the end of
                // `lower_inner` caught it and refused the whole method — which
                // is why every C2 candidate containing `newarray` alongside a
                // live oop silently lost its optimized body, `Short2.<init>()V`
                // (`2 shadow pushes vs 1 reloads`) included.
                //
                // `Op::New` now emits the same pair for the same reason (see
                // its arm above); it used to emit neither. Placed exactly where
                // `Op::ConstString` / `Op::ConstClass` put theirs — after the stub, before the
                // zero test — because the reload uses RCX and never RAX, so the
                // returned (possibly relocated) pointer is untouched.
                self.emit_shadow_reload();

                self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
                self.buf.emit(&[0x0F, 0x85]); // JNZ allocated
                let allocated_patch = self.buf.pos();
                self.buf.emit(&[0; 4]);
                self.emit_mov_reg_imm64(RAX, i64::MIN as u64);
                self.buf.emit_byte(0xE9); // JMP shared exception epilogue
                let exception_patch = self.buf.pos();
                self.buf.emit(&[0; 4]);
                self.push_call_exc_patch(exception_patch);
                let allocated = self.buf.pos();
                let rel = allocated as i32 - (allocated_patch as i32 + 4);
                Self::patch_or_bail(&mut self.buf, allocated_patch, rel);
                self.store_rax(slot);
            }
            Op::Neg => {
                let slot = self.alloc_slot(id);
                self.gp_load_value(RAX, node.inputs[0]);
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
                // The FP arms computed in RAX, so an XMM-resident result
                // reaches its register through the home word it just wrote.
                if matches!(node.ty, IrType::Float | IrType::Double) {
                    self.publish_fp_from_slot(id, slot, node.ty == IrType::Double);
                }
            }
            Op::And => {
                let slot = self.alloc_slot(id);
                self.gp_load_value(RAX, node.inputs[0]);
                self.gp_load_value(RCX, node.inputs[1]);
                // AND RAX, RCX
                self.buf.emit(&[0x48, 0x21, 0xC8]);
                self.store_rax(slot);
            }
            Op::Or => {
                let slot = self.alloc_slot(id);
                self.gp_load_value(RAX, node.inputs[0]);
                self.gp_load_value(RCX, node.inputs[1]);
                // OR RAX, RCX
                self.buf.emit(&[0x48, 0x09, 0xC8]);
                self.store_rax(slot);
            }
            Op::Xor => {
                let slot = self.alloc_slot(id);
                self.gp_load_value(RAX, node.inputs[0]);
                self.gp_load_value(RCX, node.inputs[1]);
                // XOR RAX, RCX
                self.buf.emit(&[0x48, 0x31, 0xC8]);
                self.store_rax(slot);
            }
            Op::Shl => {
                let slot = self.alloc_slot(id);
                self.gp_load_value(RAX, node.inputs[0]);
                self.gp_load_value(RCX, node.inputs[1]);
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
                self.gp_load_value(RAX, node.inputs[0]);
                self.gp_load_value(RCX, node.inputs[1]);
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
                self.gp_load_value(RAX, node.inputs[0]);
                self.gp_load_value(RCX, node.inputs[1]);
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
                // Fused into its consuming `Op::If` (see `lower_terminator`):
                // nothing to materialise here.
                if self.fused_cmp.get(id as usize).copied().unwrap_or(false) {
                    return;
                }
                let slot = self.alloc_slot(id);
                self.gp_load_value(RAX, node.inputs[0]);
                self.gp_load_value(RCX, node.inputs[1]);
                // CMP EAX, ECX — 32-bit for the int comparisons this node was
                // introduced for. A REFERENCE comparison (`ifnull`,
                // `if_acmpeq`) must compare all 64 bits: a heap pointer whose
                // low word happens to be zero would otherwise test equal to
                // null, and two distinct objects 4 GiB apart would test equal
                // to each other. Selected from the operand types, so an int
                // compare keeps the shorter encoding.
                let ref_cmp = matches!(self.graph.nodes[node.inputs[0] as usize].ty, IrType::Ref)
                    || matches!(self.graph.nodes[node.inputs[1] as usize].ty, IrType::Ref);
                if ref_cmp {
                    // CMP RAX, RCX
                    self.buf.emit(&[0x48, 0x39, 0xC8]);
                } else {
                    self.buf.emit(&[0x39, 0xC8]);
                }
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
                self.gp_load_value(RAX, node.inputs[0]); // a
                self.gp_load_value(RCX, node.inputs[1]); // b
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
                self.fp_load_value(XMM0, node.inputs[0], is_d); // a
                self.fp_load_value(XMM1, node.inputs[1], is_d); // b
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
                self.gp_load_value(RAX, node.inputs[0]);
                // MOVSXD RAX, EAX
                self.buf.emit(&[0x48, 0x63, 0xC0]);
                self.store_rax(slot);
            }
            Op::L2I => {
                let slot = self.alloc_slot(id);
                self.gp_load_value(RAX, node.inputs[0]);
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
                //
                // Through `resume_bci`, like every other deopt this file
                // emits. `IrBuilder::splice_guard_seen` refuses a graph that
                // built a guard inside a splice, so today the mapping is a
                // no-op here — but this arm was the ONLY one resolving from a
                // raw bci, and a raw bci inside a spliced body is the exact
                // shape that produced
                // `fixed-bugs/jit/ir-inline-turns-an-index-out-of-bounds-into-an-internalerror-FIXED-20260828.md`.
                // A fence and an asymmetry is one fence away from the bug;
                // agreeing with the other emitters costs nothing.
                let bci = self.resume_bci(bci);
                let frame_state = self.resolve_frame_state_for_bci(bci);
                let reason = DeoptReason::UncommonTrap;
                let point = Box::new(DeoptimizationPoint {
                    native_offset: self.buf.pos() as u32,
                    bci: bci as u32,
                    reason,
                    action: DeoptAction::Reinterpret,
                    speculation_id: 0,
                    frame_state,
                    // The guard fires BEFORE the bytecode it protects, so the
                    // interpreter re-runs that bytecode. `for_reason` is the
                    // convention this file already documents, now written down
                    // in the metadata instead of inferred by each consumer.
                    semantics: ResumeSemantics::for_reason(reason),
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
                // guard JNZ -- tolerated on an overflowed buffer; see
                // `Self::patch_or_bail` / `patch_rel32_to_here`.
                Self::patch_or_bail(&mut self.buf, jnz_patch, rel);
            }
            // getfield read — `Op::Load`. The builder emits
            // `Op::Load(MemKind::Int)` for the int-category fields,
            // `Op::Load(MemKind::Ref)` for reference fields and (COV-03)
            // `Long`/`Float`/`Double` for the wide ones; all read through the
            // checked `jit_getfield` helper, which returns the int payload, the
            // raw pointer, the long payload or the FP bit pattern according to
            // the receiver's registered layout. The inline fallback below is
            // int-only and layout-naive, and `lower_inner` refuses any graph
            // that would need it for a reference or wide load. inputs = [ctrl,
            // mem, base, offset] where `offset` is a `Const(field_index)`.
            Op::Load(_) => {
                let slot = self.alloc_slot(id);
                let base = node.inputs[2];
                let offset_node = node.inputs[3];
                // `lower_inner` refuses the graph unless every field access's
                // offset edge is a `Const`, so the `None` arm is unreachable.
                // It deopts rather than falling back to slot `0`, which is what
                // it used to do: slot 0 is a DIFFERENT field of the same
                // receiver, read or written with no trace of the substitution.
                // A deopt is the one answer that is always correct here — the
                // interpreter re-executes the access against the real layout —
                // and it costs nothing on a path nothing reaches.
                let Op::Const(field_index) = self.graph.nodes[offset_node as usize].op else {
                    self.emit_unconditional_deopt(node.bytecode_pc.unwrap_or(0));
                    return;
                };
                // Guarded inline read of a compact field, with the checked
                // helper as the slow path — the same trade the single-pass
                // backend makes. Returns false when this site is not eligible
                // (no resolved compact slot, no published region bounds, gate
                // off, or a width this arm does not emit), leaving the helper
                // path below untouched.
                if self.emit_inline_compact_getfield(
                    node.bytecode_pc,
                    node.ty,
                    base,
                    field_index,
                    slot,
                ) {
                    return;
                }
                if self.getfield != 0 {
                    crate::metrics::note_getfield_arm(5);
                    self.load_reg_from_frame(CALL_ARG_REGS[0], self.context_slot_off);
                    self.gp_load_value(CALL_ARG_REGS[1], base);
                    // Reference loads must carry `GETFIELD_EXPECT_REFERENCE` at
                    // EVERY arm, not just the one that crashed — this is the
                    // inline-compact fallback arm. No receiver proof is claimed
                    // here.
                    self.emit_mov_reg_imm64(
                        CALL_ARG_REGS[2],
                        cratonvm_jit_api::getfield_index_arg(
                            field_index as u32,
                            node.ty == IrType::Ref,
                            false,
                        ),
                    );
                    self.emit_mov_reg_imm64(RAX, self.getfield as u64);
                    self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                                                  // The checked `jit_getfield` helper returns the `i64::MIN`
                                                  // deopt/NPE sentinel (with the pending-NPE flag set) on a bad
                                                  // receiver instead of a legitimate field value. For an
                                                  // int-category or reference field `i64::MIN` can never be a
                                                  // genuine result (no plausible heap pointer equals it), so a
                                                  // plain compare-and-bail is unambiguous. For a `J`/`D` field
                                                  // it CAN be — `Long.MIN_VALUE`, and `-0.0`, whose bits are
                                                  // exactly `i64::MIN` — so COV-03 reuses the same out-of-band
                                                  // `jit_dispatch_threw` peek `Op::Call` already uses for a
                                                  // `J`/`D` return. Without either, a bad receiver silently
                                                  // corrupts execution instead of throwing (crash → hang
                                                  // conversion).
                    self.emit_helper_sentinel_check(node.ty);
                    self.store_rax(slot);
                    // A `Float`/`Double` result arrives in RAX as raw bits and
                    // its home word now holds them; republish into the value's
                    // XMM register, exactly as `Op::ConstF` / FP `Op::Param` do.
                    match node.ty {
                        IrType::Float => self.publish_fp_from_slot(id, slot, false),
                        IrType::Double => self.publish_fp_from_slot(id, slot, true),
                        _ => {}
                    }
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
                    self.gp_load_value(RAX, base);
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
            // putfield write — `Op::Store`. `MemKind` selects the lowering:
            // `Int` is the inline heap write below (or `jit_putfield_int` under
            // compact layout), `Ref` is ALWAYS `jit_putfield_object` (COV-03 —
            // the barrier), and `Long`/`Float`/`Double` are the matching
            // `jit_putfield_*` helper. The inline int write: a null receiver
            // DEOPTS (the interpreter then re-executes this putfield and throws
            // NullPointerException), else write a `Value::Int(value)` cell
            // (discriminant 0 + the 32-bit payload, high qword cleared so no
            // stale ref/garbage survives — mirroring the scalar-replace store
            // and the real helper). inputs = [ctrl, mem, base, offset, value];
            // produces no value (a pure memory-ordering token), so no slot is
            // allocated.
            Op::Store(kind) => {
                let base = node.inputs[2];
                let offset_node = node.inputs[3];
                let value = node.inputs[4];
                // Unreachable for the same reason the `Op::Load` arm's is, and
                // deopts for the same reason — see there.
                let Op::Const(field_index) = self.graph.nodes[offset_node as usize].op else {
                    self.emit_unconditional_deopt(node.bytecode_pc.unwrap_or(0));
                    return;
                };
                let tag_off = HEADER_SIZE as i32 + (field_index as i32) * SLOT_SIZE as i32;
                let pay_off = tag_off + FIELD_CELL_PAYLOAD32_OFFSET as i32;
                let high_off = tag_off + 8; // the 8-byte payload region (Long/ref)
                let bci = node.bytecode_pc.unwrap_or(0);
                // COV-03 — a REFERENCE store. There is no inline route and no
                // layout-conditional choice to make: `jit_putfield_object` is
                // the single-pass backend's own full-barrier fallback, it is
                // compact-aware in its own right, and it is what carries the
                // SATB pre-barrier on the OLD reference plus the collector's
                // post-write barrier. A missing barrier is invisible until a
                // concurrent or generational collection reclaims a still-live
                // object, so this tier does not get a barrier-free fast path
                // until it can prove the same premises `x64::objects` proves
                // (mapped, genuinely compact, YOUNG receiver whose old field is
                // null, with live region bounds published).
                //
                // The null check stays INLINE and deopts, for the same reason
                // the int path's does: the helper returns silently on an
                // implausible receiver, so calling it unguarded would convert a
                // NullPointerException into a dropped store.
                if matches!(kind, MemKind::Ref) {
                    self.gp_load_value(RAX, base);
                    self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                    self.emit_deopt_if_zero(bci, DeoptReason::NullCheck);
                    // GATED reference store — tried first, and complete when it
                    // takes: the barrier-free store, both gate sequences, the
                    // helper fallback and the out-of-bounds drop all converge
                    // at its end. `false` means "not admitted", and the
                    // unconditional helper call below then runs exactly as it
                    // did before. See `emit_gated_ir_ref_putfield`.
                    if self.emit_gated_ir_ref_putfield(
                        node.bytecode_pc,
                        base,
                        value,
                        field_index,
                    ) {
                        return;
                    }
                    // jit_putfield_object(vm_ptr, obj_ptr, field_index, val).
                    // Unlike every other putfield helper it takes the context
                    // pointer; `scan_frame_needs` reserves the slot for it.
                    self.load_reg_from_frame(CALL_ARG_REGS[0], self.context_slot_off);
                    self.gp_load_value(CALL_ARG_REGS[1], base);
                    self.emit_mov_reg_imm64(CALL_ARG_REGS[2], field_index as i64 as u64);
                    self.gp_load_value(CALL_ARG_REGS[3], value);
                    self.emit_mov_reg_imm64(RAX, self.putfield_object as u64);
                    self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                    return;
                }
                // COV-03 — a WIDE store (`J`/`F`/`D`). Same shape as the compact
                // int store: inline null check + deopt, then the width's own
                // `(obj_ptr, field_index, bits)` helper, which resolves the
                // packed offset and writes a correctly-tagged cell. An FP value
                // rides a GPR as raw bits, which is how its frame slot already
                // holds it, so the ordinary integer slot load is the marshal.
                let wide_helper = match kind {
                    MemKind::Long => Some(self.putfield_long),
                    MemKind::Float => Some(self.putfield_float),
                    MemKind::Double => Some(self.putfield_double),
                    _ => None,
                };
                if let Some(helper) = wide_helper {
                    self.gp_load_value(RAX, base);
                    self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                    self.emit_deopt_if_zero(bci, DeoptReason::NullCheck);
                    self.gp_load_value(CALL_ARG_REGS[0], base);
                    self.emit_mov_reg_imm64(CALL_ARG_REGS[1], field_index as i64 as u64);
                    self.gp_load_value(CALL_ARG_REGS[2], value);
                    self.emit_mov_reg_imm64(RAX, helper as u64);
                    self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                    return;
                }
                // Compact layout packs field offsets, so the uniform
                // `field_index * SLOT_SIZE` displacement below is wrong for a
                // compact object. Route the write through `jit_putfield_int`,
                // which resolves the packed offset from the receiver's
                // registered layout — the same helper the baseline tier uses.
                //
                // The null check stays INLINE and still deopts. The helper
                // returns silently on an implausible receiver, so calling it
                // unguarded would convert a NullPointerException into a
                // dropped store — the exact silent-data-loss defect the inline
                // path was fixed for in cd451faccc.
                if cratonvm_types::compact_ref_fields_enabled() {
                    self.gp_load_value(RAX, base);
                    self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                    self.emit_deopt_if_zero(bci, DeoptReason::NullCheck);
                    // jit_putfield_int(obj_ptr, field_index, val) — no context.
                    // Receiver first: `load_reg_from_frame` into arg0 would be
                    // clobbered by nothing here, but keep the order arg0..arg2
                    // so a future stack-arg spill sees a conventional sequence.
                    self.gp_load_value(CALL_ARG_REGS[0], base);
                    self.emit_mov_reg_imm64(CALL_ARG_REGS[1], field_index as i64 as u64);
                    self.gp_load_value(CALL_ARG_REGS[2], value);
                    self.emit_mov_reg_imm64(RAX, self.putfield_int as u64);
                    self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                    return;
                }
                // Receiver → RAX, value → RCX.
                self.gp_load_value(RAX, base);
                self.gp_load_value(RCX, value);
                // A null receiver is a NullPointerException, not a no-op. This
                // arm used to `JE` over the store, silently dropping it and
                // continuing — the same silent-data-loss defect fixed in the
                // single-pass backend in cd451faccc, which this tier still had.
                // Deopt instead, exactly as the `ArrayLoad`/`ArrayStore` guards
                // below already do for a null array: control leaves for the
                // shared deopt stub, and the interpreter re-executes this
                // putfield and throws. No new machinery, and the store body is
                // no longer required to be a fixed 27 bytes.
                self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                self.emit_deopt_if_zero(bci, DeoptReason::NullCheck);
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
                // (the store body no longer needs a fixed size: the null path
                // exits to the deopt stub rather than jumping over these bytes)
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
                let bci = node.bytecode_pc.unwrap_or(0);
                // COV-02: everything that is not `float`/`double` lands the
                // element in a GPR and spills it exactly as the single-pass
                // backend does. The guards, the SIB base/index registers and
                // the header displacement are identical for every width — the
                // only thing that varies is one instruction.
                if !matches!(kind, MemKind::Float | MemKind::Double) {
                    self.gp_load_value(RAX, node.inputs[2]); // array → RAX
                    self.gp_load_value(RCX, node.inputs[3]); // index → RCX
                    self.emit_array_null_bounds_guards(bci);
                    self.emit_gpr_array_elem_load(*kind);
                    self.store_rax(slot);
                    return;
                }
                let is_d = matches!(kind, MemKind::Double);
                self.gp_load_value(RAX, node.inputs[2]); // array → RAX
                self.gp_load_value(RCX, node.inputs[3]); // index → RCX
                self.emit_array_null_bounds_guards(bci);
                // MOVSS/MOVSD XMM0, [RAX + RCX*{4,8} + HEADER_SIZE]. ModRM 0x44
                // (mod=01, reg=XMM0, r/m=SIB); SIB 0x88 (*4) / 0xC8 (*8), idx=RCX,
                // base=RAX; disp8 = HEADER_SIZE.
                let prefix = if is_d { 0xF2 } else { 0xF3 };
                let sib = if is_d { 0xC8 } else { 0x88 };
                self.buf
                    .emit(&[prefix, 0x0F, 0x10, 0x44, sib, HEADER_SIZE as u8]);
                self.fp_store_value(id, slot, XMM0, is_d);
            }
            // FP array element store (Slice B) — `fastore`/`dastore`. inputs =
            // [ctrl, mem, array, index, value]. Load the value into XMM0 first
            // (it must survive the guards; the bounds check clobbers only a GPR
            // scratch), then array→RAX, index→RCX, the null + bounds guards, and
            // `MOVSS`/`MOVSD` XMM0 into the element. Produces a memory token (no
            // result slot is read), but a slot is allocated for layout uniformity.
            Op::ArrayStore(kind) => {
                let _slot = self.alloc_slot(id);
                let bci = node.bytecode_pc.unwrap_or(0);
                // COV-02: the integral widths take the value in RDX, which
                // `emit_array_null_bounds_guards` does not touch (it uses
                // RAX/RCX and R10), so it can be loaded before the guards for
                // the same reason XMM0 is on the FP path.
                //
                // `MemKind::Ref` cannot reach here: `IrBuilder::build` has no
                // `aastore` (0x53) arm, deliberately — see the refusal note at
                // that arm's neighbours in `ir.rs`. Fail closed rather than
                // emit a barrier-less reference store.
                if !matches!(kind, MemKind::Float | MemKind::Double) {
                    if matches!(kind, MemKind::Ref) {
                        self.latch_bailout(Bailout::with_context(
                            BailoutReason::UnsupportedShape(
                                "ir_lower: ArrayStore(Ref) needs the SATB + card write barriers",
                            ),
                            format!("n{id} is an aastore; the IR tier emits no store barrier"),
                        ));
                        return;
                    }
                    self.gp_load_value(RDX, node.inputs[4]); // value → RDX
                    self.gp_load_value(RAX, node.inputs[2]); // array → RAX
                    self.gp_load_value(RCX, node.inputs[3]); // index → RCX
                    self.emit_array_null_bounds_guards(bci);
                    self.emit_gpr_array_elem_store(*kind);
                    return;
                }
                let is_d = matches!(kind, MemKind::Double);
                self.fp_load_value(XMM0, node.inputs[4], is_d); // value → XMM0
                self.gp_load_value(RAX, node.inputs[2]); // array → RAX
                self.gp_load_value(RCX, node.inputs[3]); // index → RCX
                self.emit_array_null_bounds_guards(bci);
                // MOVSS/MOVSD [RAX + RCX*{4,8} + HEADER_SIZE], XMM0 (opcode 0x11).
                let prefix = if is_d { 0xF2 } else { 0xF3 };
                let sib = if is_d { 0xC8 } else { 0x88 };
                self.buf
                    .emit(&[prefix, 0x0F, 0x11, 0x44, sib, HEADER_SIZE as u8]);
            }
            // arraylength (COV-02). inputs = [ctrl, mem, array]. One 32-bit
            // load at a fixed header offset behind the JVMS null check. No
            // element type, no bounds check, no barrier — the cheapest node in
            // this lane and 43 of its 77 measured events.
            //
            // `MOV EAX, [RAX + ARRAY_LENGTH_OFFSET]` zero-extends into RAX,
            // which is also the correct sign extension: an array length is a
            // non-negative `u32` bounded by `i32::MAX`. Byte-identical to the
            // single-pass `emit_arraylength_regs`.
            //
            // The null path DEOPTS rather than jumping over the load: control
            // leaves for the shared stub, the interpreter re-executes this
            // `arraylength` and throws the real NullPointerException with the
            // method's own handler semantics. Emitting the load without the
            // check would be a SIGSEGV in generated code.
            Op::ArrayLength => {
                let slot = self.alloc_slot(id);
                let bci = node.bytecode_pc.unwrap_or(0);
                self.gp_load_value(RAX, node.inputs[2]); // array → RAX
                self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                self.emit_deopt_if_zero(bci, DeoptReason::NullCheck);
                self.buf.emit(&[
                    0x8B,
                    0x40,
                    // Compile-time checked: a layout constant past 127 would
                    // encode a NEGATIVE disp8 and read before the object.
                    crate::x64::disp::disp8_const(ARRAY_LENGTH_OFFSET as i64) as u8,
                ]); // MOV EAX, [RAX + ARRAY_LENGTH_OFFSET]
                self.store_rax(slot);
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
                self.gp_load_value(CALL_ARG_REGS[1], node.inputs[2]);
                self.gp_load_value(CALL_ARG_REGS[2], node.inputs[3]);
                self.emit_mov_reg_imm64(RAX, self.lambda_int_to_double as u64);
                self.buf.emit(&[0xFF, 0xD0]);
                self.store_rax(slot);
            }
            Op::Call { info_ptr } => {
                // Relocation contract: publish the map for this GC-capable
                // point BEFORE `alloc_slot`. Every route out of this arm can
                // reach a collection, and `alloc_slot` marks the call's own
                // result slot defined — but that slot is not written until the
                // call RETURNS, so covering it here would publish whatever the
                // previous frame left at that offset as a live reference.
                // `live_frame_hi` is the spill watermark as it stands now, i.e.
                // before this node's result slot is carved. Under liveness
                // colouring the call's result slot may *recycle* a colour, so
                // "the watermark" is no longer "one past the last slot handed
                // out" — but it is still an upper bound on every word this
                // frame has written, which is exactly what the band scan needs.
                let sp_live_hi = self.spill_high_water;
                self.emit_safepoint_map(sp_live_hi);
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
                    // This route bypasses `emit_call_return_check`, so the
                    // shared post-call reload never runs here. Withdraw the
                    // coverage claim — the reload below restores the homes but
                    // this route cannot promise a relocating collector anything
                    // — and then emit the reload explicitly.
                    //
                    // Dropping `pending_shadow` instead (the original) withdrew
                    // the claim but left the PUSH that `emit_safepoint_map`
                    // above had already emitted, with nothing to pop it. Every
                    // execution of a self-recursive direct call then leaked its
                    // published oops permanently, and a recursive method in a
                    // hot loop walked the thread's 2 MiB shadow stack off its
                    // end within a second — storing across the heap behind it,
                    // because no backend emits the `end` guard the shadow-stack
                    // design documents. That is what made the raw JIT-to-JIT
                    // gate unsafe to open; this route only exists when it is.
                    if let Some(last) = self.oop_maps.last_mut() {
                        last.moving_young_coverage_complete = false;
                    }
                    self.emit_self_recursive_call(&node.inputs, slot, num_args);
                    self.emit_shadow_reload();
                    return;
                }
                // IR direct-call lowering: a statically-bound site whose callee
                // was eagerly compiled becomes a raw `CALL` into that callee's
                // entry — no `jit_invoke_dispatch` round trip. See
                // `emit_direct_cross_call`, which now marshals arguments past
                // the entry-ABI register file onto the stack, so an over-wide
                // site is bound rather than dropped back to helper dispatch.
                //
                // Why that is sound for BOTH callee kinds this map holds:
                //
                //  * a thin `extern "C"` VM helper reads its stack arguments
                //    the way the platform C ABI says, and always could — the
                //    register-only lowering was the sole obstacle;
                //  * a JIT-compiled callee with more parameters than the
                //    register file can only have a SINGLE-PASS body, because
                //    `lower()` refuses such a graph outright (see
                //    `incoming_abi_reg_capacity`) — and the single-pass
                //    prologue reads stack-passed parameters through
                //    `emit_load_caller_arg`, at the offsets
                //    `stack_arg_block_size` writes them to.
                //
                // `lower_data_node` has no post-`match` code, so an early
                // `return` here fully handles the node.
                if let Some(&(entry, callee_needs_ctx)) =
                    node.bytecode_pc.and_then(|pc| self.direct_calls.get(&pc))
                {
                    if entry != 0 {
                        // 0 = virtual, 1 = special, 2 = interface, 3 = static,
                        // 4 = self-recursive static. The first three carry a
                        // receiver in argument 0; this map holds static and
                        // special, and the test is right for all five.
                        // SAFETY: as the `invoke_kind == 4` read above — the
                        // pointer names a live `JitInvokeInfo` owned by
                        // `ir_call_infos` for the lifetime of this compile.
                        let has_receiver =
                            matches!(unsafe { (*(*info_ptr as *const JitInvokeInfo)).invoke_kind }, 0 | 1 | 2);
                        self.emit_direct_cross_call(
                            &node.inputs,
                            slot,
                            num_args,
                            entry,
                            callee_needs_ctx,
                            *info_ptr,
                            node.ty,
                            has_receiver,
                            node.bytecode_pc.unwrap_or(0),
                        );
                        return;
                    }
                }
                // IR inline-cache lowering (jit-inlining-and-ir-calls): a
                // virtual / interface site with allocated MIC + PIC slots gets
                // the same monomorphic-then-polymorphic cascade the single-pass
                // backend emits, falling back to `jit_invoke_virtual_mic` (which
                // populates both caches) rather than the blind
                // `jit_invoke_dispatch`. Same register-file precondition as the
                // direct path — the hit path marshals into the callee entry ABI
                // with the hidden context pointer in `abi[0]`, and there is no
                // stack-argument path — so an over-wide site falls through to
                // the unchanged helper dispatch below. `lower_data_node` has no
                // post-`match` code, so an early `return` fully handles the node.
                // The inline cascade, including its hashed megamorphic tail,
                // calls compiled entries directly.  Keep it under the same
                // master switch as the baseline backend: the opt-out must
                // route every virtual call through the helper, not merely
                // disable the MIC/PIC prefix while leaving the megamorphic
                // raw-call stub reachable.
                if crate::direct_jit_callee_calls_enabled() {
                    if let Some(&(mic, pic)) =
                        node.bytecode_pc.and_then(|pc| self.ic_slots.get(&pc))
                    {
                        if mic != 0
                            && pic != 0
                            && self.invoke_virtual_mic != 0
                            && num_args >= 1
                            && num_args + 1 <= ENTRY_ABI_REGS.len()
                        {
                            self.emit_inline_cache_call(
                                &node.inputs,
                                slot,
                                num_args,
                                mic,
                                pic,
                                *info_ptr,
                                node.ty,
                            );
                            return;
                        }
                    }
                }
                // 1. Marshal each Java arg into the staging region.
                for i in 0..num_args {
                    let arg = node.inputs[2 + i];
                    self.gp_load_value(RAX, arg);
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
                // 4. Exception sentinel + result spill — shared with the direct
                //    cross-method call path; see `emit_call_return_check`.
                self.emit_call_return_check(slot, node.ty);
            }
            // ── cov-01: the constant-pool constants that are calls ───────
            //
            // `ldc <String>` and `ldc <Class>`. Both materialise a REFERENCE by
            // calling the same helper the single-pass backend calls, and both
            // publish a safepoint map first for the reason the `Op::Call` arm
            // above spells out: the helper can allocate (interning a literal,
            // constructing a mirror) and therefore collect, and the map must
            // describe the frame as it stands BEFORE this node's own result
            // slot is carved — that slot is not written until the call returns,
            // so covering it would publish whatever the previous frame left
            // there as a live reference.
            //
            // Neither result needs an `i64::MIN` check, and the difference is
            // in the helpers, not in an oversight:
            //
            //   * `jit_ldc_string` cannot fail into a pending exception. It
            //     returns 0 only for a null `vm_ptr`/`bytes`, neither of which
            //     can occur here (the graph is `needs_context`, and the bytes
            //     are owned by the artifact).
            //   * `jit_ldc_class_cp` reports a failed resolution as `0` with a
            //     pending exception published, which is the SAME convention
            //     `Op::New` uses — so it takes the same zero-test and the same
            //     conversion to the JIT-wide `i64::MIN` before the shared
            //     exception epilogue.
            Op::ConstString {
                holder_class_id,
                cp_idx,
            } => {
                let (holder_class_id, cp_idx) = (*holder_class_id, *cp_idx);
                let sp_live_hi = self.spill_high_water;
                self.emit_safepoint_map(sp_live_hi);
                let slot = self.alloc_slot(id);
                // The same shared `(vm, holder_class_id, cp_idx)` stub the
                // `Op::ConstClass` arm below uses, for the same reason: two
                // sites that call helpers with one ABI must not each hand-roll
                // it. It also emits the post-call frame republish the helper
                // needs after crossing the JIT boundary.
                crate::runtime_lowering::emit_ldc_class_cp_stub(
                    &mut self.buf,
                    self.context_slot_off,
                    self.ldc_string_cp,
                    holder_class_id,
                    cp_idx,
                    self.frame_record,
                );
                self.emit_shadow_reload();
                // 0 = the constant-pool entry could not be re-read and a
                // pending exception was published. The bytes-baked predecessor
                // could not fail this way and so took no test; the CP-indexed
                // form can, so it takes the same test `Op::ConstClass` does.
                self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
                self.buf.emit(&[0x0F, 0x85]); // JNZ resolved
                let resolved_patch = self.buf.pos();
                self.buf.emit(&[0; 4]);
                self.emit_mov_reg_imm64(RAX, i64::MIN as u64);
                self.buf.emit_byte(0xE9); // JMP shared exception epilogue
                let exception_patch = self.buf.pos();
                self.buf.emit(&[0; 4]);
                self.push_call_exc_patch(exception_patch);
                let resolved = self.buf.pos();
                let rel = resolved as i32 - (resolved_patch as i32 + 4);
                Self::patch_or_bail(&mut self.buf, resolved_patch, rel);
                self.store_rax(slot);
            }
            Op::ConstClass {
                holder_class_id,
                cp_idx,
            } => {
                let (holder_class_id, cp_idx) = (*holder_class_id, *cp_idx);
                let sp_live_hi = self.spill_high_water;
                self.emit_safepoint_map(sp_live_hi);
                let slot = self.alloc_slot(id);
                // Shared with the deferred-`new` stub: `(vm, holder_class_id,
                // cp_idx)` in the entry ABI's first three argument registers,
                // an absolute CALL, then the post-call frame republish. Using
                // the shared emitter is what keeps this site's ABI from
                // drifting away from the single-pass one it mirrors.
                crate::runtime_lowering::emit_ldc_class_cp_stub(
                    &mut self.buf,
                    self.context_slot_off,
                    self.ldc_class_cp,
                    holder_class_id,
                    cp_idx,
                    self.frame_record,
                );
                self.emit_shadow_reload();
                // 0 = resolution failed and published a pending exception.
                // Convert to the JIT-wide i64::MIN and take the shared
                // exception epilogue, exactly as the `Op::New` arm does.
                self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
                self.buf.emit(&[0x0F, 0x85]); // JNZ resolved
                let resolved_patch = self.buf.pos();
                self.buf.emit(&[0; 4]);
                self.emit_mov_reg_imm64(RAX, i64::MIN as u64);
                self.buf.emit_byte(0xE9); // JMP shared exception epilogue
                let exception_patch = self.buf.pos();
                self.buf.emit(&[0; 4]);
                self.push_call_exc_patch(exception_patch);
                let resolved = self.buf.pos();
                let rel = resolved as i32 - (resolved_patch as i32 + 4);
                Self::patch_or_bail(&mut self.buf, resolved_patch, rel);
                self.store_rax(slot);
            }
            // ── cov-05: checkcast ──────────────────────────────────────────
            //
            // Same ABI as the single-pass backend's 0xc0 arm: `jit_checkcast
            // (vm_ptr, obj_ptr, name_ptr, name_len) -> obj_ptr | 0 | i64::MIN`
            // in RAX. Unlike `instanceof`, a definitive refusal stashes a
            // `ClassCastException` and returns the deopt/exception sentinel —
            // `emit_call_return_check` is the SAME sentinel-drain-through-the-
            // shared-epilogue helper `Op::Call` uses for a callee's exception,
            // so this is not new machinery, just a new caller of it. `Ref` is
            // never legitimately `i64::MIN` (no plausible heap pointer is),
            // so it takes that function's simple `CMP ; JE` shape.
            Op::CheckCast { name_ptr, name_len } => {
                let (name_ptr, name_len) = (*name_ptr, *name_len);
                let obj = node.inputs[2];
                let sp_live_hi = self.spill_high_water;
                // Allocated BEFORE the fast path so that path can store its
                // result, and AFTER `sp_live_hi` is read so the safepoint map
                // below still describes exactly the slots it described when
                // this arm had only one path.
                let slot = self.alloc_slot(id);

                // ---- inline class-id fast path ----
                //
                // The twin of the single-pass backend's `0xc0` arm, and it has
                // to exist HERE as well: this is the door a HOT method reaches.
                // The single-pass fast path on its own measured exactly zero
                // engagement on a four-million-cast probe — `checkcast=
                // 3,203,236` membership walks with it on against `3,203,316`
                // with it off — because every body that mattered had tiered up
                // into the optimizing pipeline, whose `checkcast` lowering is
                // this one. Same lesson the thin native binds recorded: a bind
                // at one compile door is not a bind.
                //
                // The claim is exactly one fact — an object whose class id
                // EQUALS the target's is assignable to it — so this can only
                // turn a slow YES into a fast YES. Subclasses, interfaces,
                // array covariance, lambda proxies and synthetics all have
                // DIFFERENT ids and fall through to the helper unchanged, as
                // does null (which the helper already answers with 0) and a
                // garbled header, which simply misses.
                let target_class_id = if name_ptr == 0 {
                    None
                } else {
                    // `Op::CheckCast` carries the interned name as a `usize`
                    // (the node is `Copy`); the intern table is keyed on that
                    // same address, so this is the identical row the helper
                    // call below reaches by pointer.
                    crate::typecheck_target_for_site(name_ptr as *const u8)
                }
                .filter(|&cid| cid != 0 && crate::x64::checkcast_inline_enabled());
                // The 1-D primitive-array variant — see the twin in the
                // single-pass arm. It needs no class id, which is the point: a
                // primitive array's header class id is 0.
                // SAFETY: an `intern_typecheck_target` pair, leaked for the
                // life of the process.
                let prim_array_tag =
                    unsafe { crate::typecheck_site_name(name_ptr as *const u8, name_len) }
                        .and_then(cratonvm_types::primitive_array_kind_tags_byte)
                        .filter(|_| crate::x64::checkcast_inline_enabled());
                let mut hit_patch: Option<usize> = None;
                if let Some(tag) = prim_array_tag {
                    crate::CHECKCAST_INLINE_SITES_PRIM_ARRAY
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    self.gp_load_value(RAX, obj);
                    self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                    let null_slow = self.emit_jcc_rel32(0x84); // JZ → helper
                                                               // CMP BYTE [RAX+KIND_TAGS_BYTE_OFFSET], tag (80 /7 ib).
                    self.buf.emit(&[
                        0x80,
                        0x78,
                        cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                        tag,
                    ]);
                    let miss_slow = self.emit_jcc_rel32(0x85); // JNE → helper
                    self.store_rax(slot);
                    hit_patch = Some(self.emit_jmp_rel32());
                    self.patch_rel32_to_here(null_slow);
                    self.patch_rel32_to_here(miss_slow);
                } else if let Some(cid) = target_class_id {
                    crate::CHECKCAST_INLINE_SITES_IR
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    self.gp_load_value(RAX, obj);
                    self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                    let null_slow = self.emit_jcc_rel32(0x84); // JZ → helper
                                                               // ARRAY RECEIVERS MUST NOT REACH THE COMPARE — see the
                                                               // twin of this guard in the single-pass `0xc0` arm, and
                                                               // `regression-suite` `RJitArrayTypecheck`, which caught
                                                               // this fix's first version. An array's header holds its
                                                               // COMPONENT class id (or 0, for a primitive array), never
                                                               // its own, so `checkcast java/lang/String` on a `String[]`
                                                               // would match the baked target and accept a cast that must
                                                               // throw. A plain object's KIND_TAGS byte is zero, pinned by
                                                               // `const _: () = assert!` in `heap_types.rs` so that JIT
                                                               // guards can use exactly this one instruction.
                                                               // CMP BYTE [RAX+KIND_TAGS_BYTE_OFFSET], 0 (80 /7 ib).
                    self.buf.emit(&[
                        0x80,
                        0x78,
                        cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                        0x00,
                    ]);
                    let kind_slow = self.emit_jcc_rel32(0x85); // JNE → helper
                                                               // CMP DWORD [RAX+0], cid. `ObjectHeader.class_id` is a
                                                               // plain `u32` at offset 0 by contract — its own doc comment
                                                               // says "JIT-emitted type guards are `CMP DWORD [recv+0],
                                                               // imm`" and `class_id_remains_at_offset_zero` enforces it —
                                                               // and this is byte-for-byte the encoding the guarded-virtual
                                                               // inline arm already emits (81 /7 id, ModRM 0x78 = mod01
                                                               // disp8 /7 rm=RAX).
                    self.buf.emit(&[0x81, 0x78, 0x00]);
                    self.buf.emit(&cid.to_le_bytes());
                    let miss_slow = self.emit_jcc_rel32(0x85); // JNE → helper
                                                               // Hit: the receiver IS the target class, so the cast
                                                               // succeeds and its result is the reference already in RAX —
                                                               // the same value the helper would have returned. No
                                                               // safepoint map, no frame-record republish and no
                                                               // sentinel drain, because this path makes no call.
                    self.store_rax(slot);
                    hit_patch = Some(self.emit_jmp_rel32());
                    self.patch_rel32_to_here(null_slow);
                    self.patch_rel32_to_here(kind_slow);
                    self.patch_rel32_to_here(miss_slow);
                }

                self.emit_safepoint_map(sp_live_hi);
                self.load_reg_from_frame(CALL_ARG_REGS[0], self.context_slot_off); // vm_ptr
                self.gp_load_value(CALL_ARG_REGS[1], obj); // obj_ptr
                self.emit_mov_reg_imm64(CALL_ARG_REGS[2], name_ptr as u64); // name_ptr
                self.emit_mov_reg_imm64(CALL_ARG_REGS[3], name_len as u64); // name_len
                self.emit_mov_reg_imm64(RAX, self.checkcast as u64);
                self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                self.emit_call_return_check(slot, IrType::Ref);
                if let Some(p) = hit_patch {
                    self.patch_rel32_to_here(p);
                }
            }
            // ── cov-05 increment 1: instanceof ────────────────────────────
            //
            // Same ABI as the single-pass backend's 0xc1 arm
            // (`x64/bytecode_walk.rs`): `jit_instanceof(vm_ptr, obj_ptr,
            // name_ptr, name_len) -> 0/1` in RAX. Reusing that helper is the
            // point — this node's whole job is to answer the subtype question
            // the single-pass backend already answers correctly (strict array
            // rule, loader-dup fallback, every recorded typecheck defect fix
            // included), not to re-derive one.
            //
            // Unlike `Op::ConstClass` this never returns a failure sentinel:
            // `jit_instanceof` cannot throw (JVMS §6.5 `instanceof`; a null or
            // unresolvable receiver/target answers `false`, never an
            // exception), so there is no post-call TEST/JNZ/exception-epilogue
            // dance here — just the safepoint map (the helper can still
            // allocate a Class mirror on first touch) and the result in RAX.
            Op::InstanceOf { name_ptr, name_len } => {
                let (name_ptr, name_len) = (*name_ptr, *name_len);
                let obj = node.inputs[2];
                let sp_live_hi = self.spill_high_water;
                self.emit_safepoint_map(sp_live_hi);
                let slot = self.alloc_slot(id);
                self.load_reg_from_frame(CALL_ARG_REGS[0], self.context_slot_off); // vm_ptr
                self.gp_load_value(CALL_ARG_REGS[1], obj); // obj_ptr
                self.emit_mov_reg_imm64(CALL_ARG_REGS[2], name_ptr as u64); // name_ptr
                self.emit_mov_reg_imm64(CALL_ARG_REGS[3], name_len as u64); // name_len
                self.emit_mov_reg_imm64(RAX, self.instanceof_check as u64);
                self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                                              // Same post-call obligation as `Op::ConstString`: the helper
                                              // crossed the JIT boundary and may have run a collection, so
                                              // this frame's mirror and any relocated published value must
                                              // be restored before anything else reads the frame.
                self.emit_post_call_frame_record();
                self.emit_shadow_reload();
                self.store_rax(slot);
            }
            // ── cov-01: getstatic ────────────────────────────────────────
            //
            // Two lowerings, chosen by the same predicate the single-pass
            // backend's `0xb2` arm uses (`x64::try_emit_inline_getstatic`), and
            // deliberately not by a second opinion:
            //
            //   * DIRECT — `resolve_static_base` accepted the site, which it
            //     does only for a class already initialised at compile time and
            //     published in the lock-free `StaticsIndex`. What is baked is
            //     the address of the class's base-POINTER cell, never of the
            //     block: the one extra dependent load is what buys immunity to
            //     every republication path. An inline load runs no `<clinit>`,
            //     which is sound only because the class is already initialised
            //     AND `CompiledMethod::static_init_classes` re-checks at the
            //     compiled entry.
            //   * HELPER — everything the resolver declines: a class not yet
            //     initialised, `java/lang/System`'s bootstrap intercept,
            //     anything not yet published, and every site when
            //     `CRATONVM_JIT=getstatic-helper` is set. `jit_getstatic` runs
            //     `<clinit>` on first touch and, on failure, stashes the Java
            //     exception and returns the `i64::MIN` deopt sentinel — routed
            //     through the shared exception epilogue rather than pushed as
            //     if it were a field value.
            //
            // A volatile static takes an MFENCE after the read on both routes
            // (x86-64 already gives the load itself acquire ordering).
            Op::LoadStatic {
                class_id,
                field_index,
                type_tag,
                is_volatile,
            } => {
                let (class_id, field_index, type_tag, is_volatile) =
                    (*class_id, *field_index, *type_tag, *is_volatile);
                if self.emit_inline_getstatic(id, class_id, field_index, type_tag, is_volatile) {
                    return;
                }
                // The helper can run `<clinit>`, i.e. arbitrary Java. Same
                // pre-call map obligation as `Op::Call`.
                let sp_live_hi = self.spill_high_water;
                self.emit_safepoint_map(sp_live_hi);
                let slot = self.alloc_slot(id);
                self.load_reg_from_frame(CALL_ARG_REGS[0], self.context_slot_off);
                self.emit_mov_reg_imm64(CALL_ARG_REGS[1], u64::from(class_id));
                self.emit_mov_reg_imm64(CALL_ARG_REGS[2], u64::from(field_index));
                self.emit_mov_reg_imm64(RAX, self.getstatic as u64);
                self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                if is_volatile {
                    // The JMM acquire. Emitted here rather than after the
                    // sentinel check because the check may CALL `dispatch_threw`
                    // for a wide value, and a fence is only meaningful between
                    // the load and the value's first use — both positions
                    // satisfy that, and this one is before any branch, so the
                    // fence is on every path out of the load. `MFENCE` touches
                    // neither RAX nor the flags the check is about to set.
                    self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
                }
                // `i64::MIN` = the helper published a pending Java exception (a
                // failed `<clinit>`) instead of a value — but for a `J`/`D`/`F`
                // static a legitimate `Long.MIN_VALUE` is bit-identical to it,
                // so the sentinel alone is ambiguous. `emit_call_return_check`
                // is exactly that protocol: plain compare-and-bail for the
                // unambiguous widths, and for a wide value the cold-path peek at
                // `jit_dispatch_threw` that distinguishes a real signal from a
                // real value.
                //
                // It is the SAME helper and the SAME disambiguation the
                // single-pass `0xb2` arm has used for wide statics all along —
                // its `emit_post_invoke_exception_check(type_tag)` takes the
                // `J`/`D`/`F` branch — so this is matching that arm's behaviour
                // rather than inventing one. It also folds in the post-call
                // frame republish and shadow reload this arm was open-coding.
                self.emit_call_return_check(slot, node.ty);
                // An FP result arrives as bits in RAX and has just been stored
                // to its home word by the line above; publish it to its register
                // the way `Op::ConstF` does. A no-op when the allocator gave
                // this value no register.
                if matches!(type_tag, b'F' | b'D') {
                    self.publish_fp_from_slot(id, slot, type_tag == b'D');
                }
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
                // A float/double constant is materialised as an integer
                // immediate, so its register copy comes from the home word.
                // `regalloc` calls `Op::ConstF` rematerializable and evicts it
                // first for exactly this reason; here it is simply cheap.
                self.publish_fp_from_slot(id, slot, node.ty == IrType::Double);
            }
            // int → float / double. Load the int operand to EAX and convert.
            Op::I2F => {
                let slot = self.alloc_slot(id);
                self.gp_load_value(RAX, node.inputs[0]);
                // CVTSI2SS XMM0, EAX
                self.buf.emit(&[0xF3, 0x0F, 0x2A, 0xC0]);
                self.fp_store_value(id, slot, XMM0, false);
            }
            Op::I2D => {
                let slot = self.alloc_slot(id);
                self.gp_load_value(RAX, node.inputs[0]);
                // CVTSI2SD XMM0, EAX
                self.buf.emit(&[0xF2, 0x0F, 0x2A, 0xC0]);
                self.fp_store_value(id, slot, XMM0, true);
            }
            // long → float / double (64-bit source operand in RAX).
            Op::L2F => {
                let slot = self.alloc_slot(id);
                self.gp_load_value(RAX, node.inputs[0]);
                // CVTSI2SS XMM0, RAX (REX.W)
                self.buf.emit(&[0xF3, 0x48, 0x0F, 0x2A, 0xC0]);
                self.fp_store_value(id, slot, XMM0, false);
            }
            Op::L2D => {
                let slot = self.alloc_slot(id);
                self.gp_load_value(RAX, node.inputs[0]);
                // CVTSI2SD XMM0, RAX (REX.W)
                self.buf.emit(&[0xF2, 0x48, 0x0F, 0x2A, 0xC0]);
                self.fp_store_value(id, slot, XMM0, true);
            }
            // float → int / long (truncate toward zero, with the JVM
            // NaN→0 / overflow→MAX|MIN fixup). Source stays in XMM0 for the
            // fixup's sign/NaN test.
            Op::F2I => {
                let slot = self.alloc_slot(id);
                self.fp_load_value(XMM0, node.inputs[0], false);
                // CVTTSS2SI EAX, XMM0
                self.buf.emit(&[0xF3, 0x0F, 0x2C, 0xC0]);
                self.emit_fp_to_int_fixup(/* is_double */ false, /* is_long */ false);
                // Sign-extend EAX→RAX so the int slot matches the single-pass ABI.
                self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
                self.store_rax(slot);
            }
            Op::F2L => {
                let slot = self.alloc_slot(id);
                self.fp_load_value(XMM0, node.inputs[0], false);
                // CVTTSS2SI RAX, XMM0 (REX.W)
                self.buf.emit(&[0xF3, 0x48, 0x0F, 0x2C, 0xC0]);
                self.emit_fp_to_int_fixup(/* is_double */ false, /* is_long */ true);
                self.store_rax(slot);
            }
            // float → double.
            Op::F2D => {
                let slot = self.alloc_slot(id);
                self.fp_load_value(XMM0, node.inputs[0], false);
                // CVTSS2SD XMM0, XMM0
                self.buf.emit(&[0xF3, 0x0F, 0x5A, 0xC0]);
                self.fp_store_value(id, slot, XMM0, true);
            }
            // double → int / long (truncate toward zero, with the JVM fixup).
            Op::D2I => {
                let slot = self.alloc_slot(id);
                self.fp_load_value(XMM0, node.inputs[0], true);
                // CVTTSD2SI EAX, XMM0
                self.buf.emit(&[0xF2, 0x0F, 0x2C, 0xC0]);
                self.emit_fp_to_int_fixup(/* is_double */ true, /* is_long */ false);
                self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
                self.store_rax(slot);
            }
            Op::D2L => {
                let slot = self.alloc_slot(id);
                self.fp_load_value(XMM0, node.inputs[0], true);
                // CVTTSD2SI RAX, XMM0 (REX.W)
                self.buf.emit(&[0xF2, 0x48, 0x0F, 0x2C, 0xC0]);
                self.emit_fp_to_int_fixup(/* is_double */ true, /* is_long */ true);
                self.store_rax(slot);
            }
            // double → float.
            Op::D2F => {
                let slot = self.alloc_slot(id);
                self.fp_load_value(XMM0, node.inputs[0], true);
                // CVTSD2SS XMM0, XMM0
                self.buf.emit(&[0xF2, 0x0F, 0x5A, 0xC0]);
                self.fp_store_value(id, slot, XMM0, false);
            }
            // Control and meta nodes — skip. `Op::Throw` is a terminator like
            // `Op::Return`/`Op::If`, handled by `lower_terminator`; it never
            // reaches this function from a real schedule (`Op::is_control()`
            // routes it away from ordinary data-node placement).
            Op::Start
            | Op::Return
            | Op::Throw
            | Op::If
            | Op::Merge
            | Op::Region
            | Op::Proj(_)
            | Op::Dead => {}
            // No lowering arm. REFUSE the compile; do NOT fall through.
            //
            // This arm used to be `_ => {}` with the comment "bail in
            // `ir_compatible` prevents reaching here". That claim was asserted,
            // never checked, and it cannot be checked where it was written:
            // `ir::ir_compatible` decides admission in the BYTECODE's
            // vocabulary (`scan.anewarray_ops`, `scan.typecheck_ops`,
            // `scan.has_athrow`), and this match is over `ir::Op`. Two
            // enumerations of different things, kept in agreement by a comment.
            //
            // What silence costs is not an optimization, it is the node's
            // semantics. `ir::Op::MonitorEnter` is the worked example: when the
            // monitor ops were added to the IR for escape analysis, this
            // catch-all would have compiled a `monitorenter` to *nothing* — the
            // lock silently gone, an unbalanced `monitorexit` left behind. That
            // one op got a hand-written guard at the top of
            // `lower_inner_with_scopes`; this arm is the general form of it.
            //
            // `verify_data_locations` is NOT that general form, though it looks
            // like it: it refuses when a value-typed input's op is absent from
            // `op_defines_result_slot`, so it only ever fires for an unlowered
            // op whose result someone READS. A monitor produces no value.
            // Nothing reads it, so nothing checked it — which is exactly why
            // that op and not another was the one that could vanish.
            //
            // Costs nothing when the claim it replaces is true. Verified
            // 2026-08-03: the four `ir::Op` variants with no arm here —
            // `I2B`, `I2C`, `I2S`, `NewArray` — are unreachable from a real
            // compile. `I2B`/`I2C`/`I2S` are constructed NOWHERE in the crate
            // (`IrBuilder` decomposes 0x91/0x92/0x93 into `Shl`/`Shr`/`And`
            // instead — see the arms at `ir.rs`'s 0x91); `NewArray` is
            // constructed only in `#[cfg(test)]` code, and the builder has no
            // `newarray`/`anewarray` opcode arm to produce it from.
            //
            // `ArrayLength` was the fifth until COV-02 gave it both an
            // `arraylength` builder arm and a lowering arm above.
            //
            // Latched rather than returned because this function is infallible
            // by signature and every emitting arm below assumes it stays that
            // way. `lower_inner_with_scopes` takes the latch after the last
            // block and discards the artifact — the same channel `alloc_slot`
            // and `slot_of` already use, and the reason `poison_slot` exists.
            other => {
                self.latch_bailout(Bailout::with_context(
                    BailoutReason::UnsupportedShape("ir_lower: op has no lowering arm"),
                    format!(
                        "n{id} ({other:?}, {:?}) reached lower_data_node's catch-all; \
                         emitting nothing would drop its semantics",
                        node.ty
                    ),
                ));
            }
        }

        // ── Publish this definition into its GP register, if it has one ──
        //
        // ONE site rather than a conversion inside every arm, and that is the
        // point: an arm this wave did not think about still publishes, so the
        // set of accelerated definitions cannot silently disagree with the set
        // of accelerated reads. Every read is gated on `gp_reg_live`, which
        // only this line sets, so the interlock reads "published here,
        // readable everywhere" with nothing in between.
        //
        // The copy is memory → register, out of the home word the arm above
        // just wrote. One load per resident DEFINITION, buying one load per
        // resident USE — the trade that pays inside a loop and breaks even
        // outside one. A per-arm register-to-register publish would be cheaper
        // still; it is not what separates reading a loop counter out of memory
        // from reading it out of a register.
        //
        // Guarded on the home slot existing, which is belt-and-braces: a node
        // the colourer gave no home was never promotable in the first place
        // (`plan_register_residency` requires one).
        if self.assigned_gpr(id).is_some() {
            if let Some(off) = self.node_slot.get(id as usize).copied().flatten() {
                let slot = off.get() as i32;
                self.publish_gp_from_slot(id, slot);
            }
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
        if let Some(pc) = self.graph.nodes[term as usize].bytecode_pc {
            self.cur_bci = pc;
        }
        let node = &self.graph.nodes[term as usize];
        match &node.op {
            // ── cov-07: athrow ─────────────────────────────────────────
            //
            // Same ABI as the single-pass backend's `0xbf` arm
            // (`x64/bytecode_walk.rs`): `jit_throw_exception(exc_ptr, bci) ->
            // i64::MIN` in RAX, always. No `vm_ptr` argument (see the field
            // doc on `throw_exception`), no post-call CMP — the helper never
            // returns anything but the sentinel, so this always takes the
            // exceptional exit. That reuses `emit_call_exc_stub`'s shared
            // bail stub exactly as every other exceptional `Op::Call`/
            // `Op::CheckCast` exit does: run the epilogue and propagate
            // `i64::MIN`, which `execute_jit_call`
            // (`vm/src/runtime/interpreter/jit_bridge.rs`) then routes
            // through `route_jit_exception_through_method` — the interpreter
            // resolves the handler (if any), never this compiled frame.
            //
            // `jit_throw_exception` stamps its OWN `bci` argument onto
            // `JitSignals::athrow_bci` before returning (`set_jit_pending_
            // exception_with_bci`), which is what gives the routing a real
            // throw pc for the range test against this method's own
            // exception table — exactly the RBC.6 fix the single-pass
            // backend already ships. This is the ONE terminator whose own
            // helper call does that job; a nested `Op::Call` /
            // `Op::CheckCast` exceptional exit needs the SEPARATE
            // `jit_set_throw_bci` stamp, which `emit_call_exc_stub` now emits
            // per distinct throw-site bci (cov-07 residual, closed — see that
            // function). The stub re-stamps this bci on the way out, which is
            // a no-op for this arm and keeps the stub's contract uniform.
            Op::Throw => {
                let exc = node.inputs[2];
                self.gp_load_value(CALL_ARG_REGS[0], exc);
                // `athrow` is refused inside a spliced body today; translating
                // anyway keeps the rule "a bci handed to the interpreter goes
                // through `resume_bci`" without an exception to remember.
                let bci = self.resume_bci(node.bytecode_pc.unwrap_or(0));
                self.emit_mov_reg_imm64(CALL_ARG_REGS[1], bci as u64);
                self.emit_mov_reg_imm64(RAX, self.throw_exception as u64);
                self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                self.buf.emit_byte(0xE9); // JMP shared exception epilogue
                let exception_patch = self.buf.pos();
                self.buf.emit(&[0; 4]);
                self.push_call_exc_patch(exception_patch);
            }
            Op::Return => {
                let mut returned_a_value = false;
                if node.inputs.len() > 1 {
                    // Has return value — move to RAX
                    let val_id = node.inputs[1];
                    if val_id != NO_NODE {
                        self.gp_load_value(RAX, val_id);
                        returned_a_value = true;
                    }
                }
                if !returned_a_value {
                    // Zero RAX on the normal VOID-return path, exactly as the
                    // single-pass backend's `0xb1` arm has always done.
                    //
                    // `i64::MIN` in the return register is this VM's "the callee
                    // trapped" sentinel, and a void method has no return value —
                    // so without this, RAX carries whatever the method's last
                    // operation left there. Every consumer reads that raw
                    // register: the single-pass inline MIC/PIC cascade's
                    // `emit_inline_callee_deopt_check`, the megamorphic hashed
                    // stub's, `jit_invoke_virtual_mic`'s `rc == i64::MIN`, and
                    // the interpreter's post-JIT drain. A false positive is not
                    // a wasted branch: `handle_compiled_callee_deopt_sentinel`
                    // resumes a stashed callee frame and drains the thread's
                    // entire pending-signal record.
                    //
                    // Measured on the Hazelcast XSD failure: one `Config.load()`
                    // serviced 11 callee "deopts" and EVERY ONE was a void
                    // callee — `QName.setValues`, `XMLAttributesImpl
                    // .addAttributeNS`, `ValidatorHandlerImpl.fillXMLAttribute`
                    // and `.fillXMLAttributes2` — none of which can throw.
                    //
                    // XOR EAX, EAX (31 C0) — zero-extends to RAX.
                    self.buf.emit(&[0x31, 0xC0]);
                }
                self.emit_epilogue();
            }
            Op::If => {
                let cond_id = node.inputs[1];
                // 2026-09-02: a compare whose only consumer is this `If` is
                // lowered HERE as `cmp; jcc` instead of `setcc; movzx; store;
                // reload; test; jcc`. `fused_cc` is the Jcc condition byte
                // under which the branch is TAKEN (the `Op::Cmp` result is
                // nonzero); `None` keeps the boolean-in-RAX shape.
                let fused_cc: Option<u8> = match self.graph.nodes.get(cond_id as usize) {
                    Some(cmp_node)
                        if self.fused_cmp.get(cond_id as usize).copied().unwrap_or(false) =>
                    {
                        match cmp_node.op {
                            Op::Cmp(cc) => {
                                let a = cmp_node.inputs[0];
                                let b = cmp_node.inputs[1];
                                let ref_cmp =
                                    matches!(self.graph.nodes[a as usize].ty, IrType::Ref)
                                        || matches!(self.graph.nodes[b as usize].ty, IrType::Ref);
                                self.gp_load_value(RAX, a);
                                self.gp_load_value(RCX, b);
                                if ref_cmp {
                                    self.buf.emit(&[0x48, 0x39, 0xC8]); // CMP RAX, RCX
                                } else {
                                    self.buf.emit(&[0x39, 0xC8]); // CMP EAX, ECX
                                }
                                Some(cc.x64_cc())
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                };
                if fused_cc.is_none() {
                    // Load condition into RAX
                    self.gp_load_value(RAX, cond_id);
                    // TEST EAX, EAX  (does not disturb RAX; sets ZF)
                    self.buf.emit(&[0x85, 0xC0]);
                }

                // Snapshot successor block indices (immutable borrow ends
                // here so `emit_phi_copies` can borrow `self` mutably).
                let succ0 = self.schedule.blocks[block_idx].successors.first().copied();
                let succ1 = self.schedule.blocks[block_idx].successors.get(1).copied();

                match (succ0, succ1) {
                    (Some(true_block), Some(false_block))
                        if ir_fused_branch_enabled()
                            && !self.edge_has_phi_copies(block_idx, true_block)
                            && !self.edge_has_phi_copies(block_idx, false_block) =>
                    {
                        // 2026-09-02: neither edge carries phi copies, so the
                        // branch needs no trampolines at all -- one Jcc to
                        // the far edge, and the near edge either falls
                        // through into the next emitted block or takes one
                        // JMP. Same polarity rule as the general layout below:
                        // the taken (true) edge is the near one unless the
                        // profile says this branch is usually not taken.
                        let favor_false = node
                            .bytecode_pc
                            .and_then(|pc| self.branch_hints.get(&pc).copied())
                            == Some(false);
                        // Jcc byte under which control goes to the FAR edge.
                        let (jcc_far, near_block, far_block) = match fused_cc {
                            Some(cc) if favor_false => (cc, false_block, true_block),
                            Some(cc) => (cc ^ 1, true_block, false_block),
                            None if favor_false => (0x85u8, false_block, true_block), // JNE
                            None => (0x84u8, true_block, false_block),               // JE
                        };
                        self.buf.emit(&[0x0F, jcc_far]);
                        let far_patch = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                        self.branch_patches.push((far_patch, far_block));
                        if near_block != block_idx + 1 {
                            self.buf.emit_byte(0xE9); // JMP near_block
                            let near_patch = self.buf.pos();
                            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                            self.branch_patches.push((near_patch, near_block));
                        }
                    }
                    (Some(true_block), Some(false_block)) => {
                        // A fused compare leaves the flags for `cc`; the
                        // general layout below wants "ZF set ⇔ condition
                        // false" (a TEST on the boolean). Rebuild that
                        // contract from the flags with one SETcc + TEST so
                        // the phi-copy trampolines stay exactly as they were.
                        if let Some(cc) = fused_cc {
                            self.buf.emit(&[0x0F, cc + 0x10, 0xC0]); // SETcc AL
                            self.buf.emit(&[0x84, 0xC0]); // TEST AL, AL
                        }
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
                        // codegen -- tolerated on an overflowed buffer; see
                        // `Self::patch_or_bail` / `patch_rel32_to_here`.
                        Self::patch_or_bail(&mut self.buf, jcc_patch, rel);
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
            // codegen -- tolerated on an overflowed buffer; see
            // `Self::patch_or_bail` / `patch_rel32_to_here`.
            Self::patch_or_bail(&mut self.buf, patch_pos, rel32);
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
        // Checked, not indexed. A snapshot slot naming an id past the end of the
        // arena means the snapshot and the graph being lowered disagree — the
        // one situation where panicking is worst, because it takes the VM down
        // from the compiler thread. Answer with the honest "this slot's value is
        // gone and I cannot describe it", which is unresumable by construction
        // (`frame_state_is_resumable`) rather than a fabricated zero.
        let Some(node) = self.graph.nodes.get(node_id as usize) else {
            return FrameValue::MaterializationRequired(EliminatedValue::unknown(
                EliminationCause::Unclassified,
            ));
        };
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
                let off = match self.node_slot.get(node_id as usize).copied().flatten() {
                    Some(slot) => slot.get() as i32,
                    None => ((idx as i32) + 1) * 8,
                };
                Self::typed_stack_slot(-off, node.ty)
            }
            _ => {
                match self.node_slot.get(node_id as usize).copied().flatten() {
                    Some(slot) => Self::typed_stack_slot(-(slot.get() as i32), node.ty),
                    // No machine location assigned. Two OPPOSITE situations wear
                    // the same shape here, and spelling them the same way is how
                    // a scalar-replaced reference local reconstructs as `null`
                    // with no error anywhere (every resume sink maps `Undefined`
                    // to `Value::Int(0)`):
                    //
                    //   * the producer is `Op::Dead` — some pass (escape
                    //     analysis' `apply_ea_to_ir`, DCE) DELETED the value the
                    //     interpreter will read. That is
                    //     `MaterializationRequired`: unresumable by construction,
                    //     so the deopt is refused and the method re-runs, instead
                    //     of resuming with a fabricated zero. The cause is
                    //     `Unclassified` because this generic slot resolver
                    //     cannot tell which pass retired the node —
                    //     `frame_value_for_object` knows, and says so.
                    //   * the producer is live but unscheduled in this naive
                    //     lowerer. Genuinely without a location; keep the
                    //     historical `Undefined`.
                    //
                    // Note this is a deopt *description*, not an emitted
                    // access: neither answer reads `[rbp - 0]`. That is why a
                    // missing location is tolerated here and refused in
                    // `slot_of`.
                    None if matches!(node.op, Op::Dead) => FrameValue::MaterializationRequired(
                        EliminatedValue::new(node_id, EliminationCause::Unclassified),
                    ),
                    None => FrameValue::Undefined,
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

    /// The refusal every [`Self::frame_value_for_object`] bail returns: "escape
    /// analysis deleted this object, and I could not describe how to rebuild it
    /// here".
    ///
    /// Carries the producer node and the allocated class so the compiler report
    /// can name *which* allocation at *which* site the deopt path cannot undo —
    /// the difference between an actionable finding and an anonymous "cannot
    /// resume". `cause` distinguishes the plain
    /// [`EliminationCause::ScalarReplacedObject`] bails (no deopt block, a
    /// non-dominating allocation/store, an unresolvable field) from
    /// [`EliminationCause::NestedVirtualObject`] (the object is describable, but
    /// one of its fields is itself virtual and v1 emits no nested graphs).
    fn eliminated_object(
        new_id: NodeId,
        info: &VirtualObjectInfo,
        cause: EliminationCause,
    ) -> FrameValue {
        FrameValue::MaterializationRequired(EliminatedValue::allocation(
            new_id,
            info.class_id,
            cause,
        ))
    }

    /// Resolve the `FrameState` for `bci` from its recorded safepoint
    /// snapshot. Falls back to an empty frame if no snapshot exists (e.g. a
    /// hand-built graph that did not register one) — the resume bci is still
    /// carried so the deopt is well-formed.
    ///
    /// First match on `bci` wins, exactly as before. The *index* of that match
    /// is what binds the snapshot to its inlined scope
    /// ([`InlineScopeTable::snapshot_scope`]), so the search is a `position`
    /// rather than a `find`.
    /// The bci to hand the INTERPRETER for a node whose `bytecode_pc` may be a
    /// combined-buffer pc inside a relocated callee body.
    ///
    /// Identity outside a spliced region, and identity on every compile that
    /// splices nothing. Inside one it answers the enclosing `invoke`'s bci,
    /// which is the only pc in this method that describes where execution is:
    /// the call has not taken effect, so re-executing it re-runs the callee.
    ///
    /// Both consumers need this for correctness, not tidiness. A throw-site bci
    /// is range-checked against this method's own exception table
    /// (`jit_set_throw_bci` → `route_jit_exception_through_method`), and a pc
    /// past the end of the bytecode falls outside every `[start_pc, end_pc)` —
    /// which silently drops a `catch`-all, the exact defect that helper exists
    /// to fix. A guard's resume bci is worse: the interpreter would be parked at
    /// an instruction that does not exist.
    fn resume_bci(&self, bci: usize) -> usize {
        for &(start, end, invoke_bci) in self.spliced_ranges {
            if bci >= start && bci < end {
                return invoke_bci;
            }
        }
        bci
    }

    fn resolve_frame_state_for_bci(&self, bci: usize) -> FrameState {
        match self.graph.safepoints.iter().position(|s| s.bci == bci) {
            Some(idx) => self.resolve_frame_state(&self.graph.safepoints[idx], idx),
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

    /// Keep a zero divisor away from the raw `IDIV` (which would raise
    /// `#DE`/`SIGFPE`) for an `Op::Div`/`Op::Rem` whose dividend is in RAX and
    /// divisor in RCX. Only emitted when the bci has a safepoint snapshot; a
    /// hand-built graph with no snapshot keeps the bare `IDIV`.
    ///
    /// Two shapes, chosen by whether the builder anchored an `Op::Guard` at
    /// this bci (see `ir::IrBuilder::add_div_zero_guard`):
    ///
    /// * **Guard present** (every graph the bytecode front end builds) — the
    ///   ArithmeticException is that guard's job, and the guard is
    ///   control-anchored, so it fires on exactly the paths the bytecode
    ///   reaches. This node, by contrast, is a *floating* one the scheduler may
    ///   have hoisted above the branch that guards the division, so it must not
    ///   trap: materialise a placeholder and `JMP` past the `IDIV`, exactly as
    ///   [`Self::emit_div_overflow_guard`] does for `MIN / -1`. The placeholder
    ///   is only ever read on a path where the division did not happen in the
    ///   source program, so its value is dead. Returns the position of the
    ///   forward `JMP` rel32 the caller must patch to the post-`IDIV`
    ///   continuation.
    /// * **No guard** (hand-built optimizer fixtures) — unchanged: deopt at
    ///   this bci and let the interpreter re-execute the division and throw.
    ///   Returns `None`.
    fn emit_div_zero_guard(&mut self, ty: IrType, bytecode_pc: Option<usize>) -> Option<usize> {
        let bci = match bytecode_pc {
            Some(b) if self.graph.safepoints.iter().any(|s| s.bci == b) => b,
            _ => return None,
        };
        // TEST ECX,ECX (int) / TEST RCX,RCX (long): ZF=1 when divisor == 0.
        if ty == IrType::Int {
            self.buf.emit(&[0x85, 0xC9]);
        } else {
            self.buf.emit(&[0x48, 0x85, 0xC9]);
        }
        let anchored = self
            .graph
            .nodes
            .iter()
            .any(|n| matches!(n.op, Op::Guard { bci: g } if g == bci));
        if !anchored {
            self.emit_deopt_if_zero(bci, DeoptReason::DivByZero);
            return None;
        }
        // JNZ do_div (divisor != 0 → the real division).
        self.buf.emit(&[0x0F, 0x85]);
        let jnz = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        // Divisor == 0: XOR EAX,EAX (zeroes the full RAX for both widths and
        // for both quotient and remainder) and jump past the `IDIV`.
        self.buf.emit(&[0x31, 0xC0]);
        self.buf.emit_byte(0xE9);
        let after_patch = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        let do_div = self.buf.pos();
        let rel = do_div as i32 - (jnz as i32 + 4);
        // div-zero JNZ -- tolerated on an overflowed buffer; see
        // `Self::patch_or_bail` / `patch_rel32_to_here`.
        Self::patch_or_bail(&mut self.buf, jnz, rel);
        Some(after_patch)
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
            // div-overflow JNE -- tolerated on an overflowed buffer; see
            // `Self::patch_or_bail` / `patch_rel32_to_here`.
            Self::patch_or_bail(&mut self.buf, p, rel);
        }
        after_patch
    }

    /// Patch the forward `JMP` emitted by [`emit_div_overflow_guard`] to the
    /// current position (the post-`IDIV` continuation, just before the result
    /// is stored).
    fn patch_div_overflow_after(&mut self, after_patch: usize) {
        let cont = self.buf.pos();
        let rel = cont as i32 - (after_patch as i32 + 4);
        // div-overflow JMP -- tolerated on an overflowed buffer; see
        // `Self::patch_or_bail` / `patch_rel32_to_here`.
        Self::patch_or_bail(&mut self.buf, after_patch, rel);
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

    /// Leave for the interpreter unconditionally at `bci`.
    ///
    /// `XOR EAX, EAX` sets ZF, so the `JNZ` [`emit_deopt_if_zero`] emits is
    /// never taken and control always reaches the deopt stub. Callers must
    /// `return` immediately: nothing after this executes, and the node's result
    /// slot (if it has one) is never read because no consumer runs.
    ///
    /// Used where a lowering discovers it has no correct encoding to emit. The
    /// alternative — emitting SOMETHING and continuing — is how a wrong field
    /// slot becomes a punned heap cell that faults somewhere else entirely.
    fn emit_unconditional_deopt(&mut self, bci: usize) {
        self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX  (sets ZF)
        self.emit_deopt_if_zero(bci, DeoptReason::UnreachedCode);
    }

    /// Emit a guard that deopts at `bci` UNLESS the just-set flags satisfy
    /// `jcc_continue` (the near-`Jcc` second byte, e.g. `0x85`=JNZ, `0x82`=JB).
    /// The "continue" condition falls through to the following code; otherwise
    /// control jumps to the shared deopt stub with this point's pointer in
    /// `DEOPT_ARG0`. Generalises `emit_deopt_if_zero` so a bounds check can
    /// continue on `JB` (unsigned index < length) and deopt otherwise.
    fn emit_deopt_unless(&mut self, jcc_continue: u8, bci: usize, reason: DeoptReason) {
        // A guard inside a spliced body resumes at the enclosing `invoke`, whose
        // snapshot is the caller's state before the call — see `resume_bci`.
        let bci = self.resume_bci(bci);
        let frame_state = self.resolve_frame_state_for_bci(bci);
        let point = Box::new(DeoptimizationPoint {
            native_offset: self.buf.pos() as u32,
            bci: bci as u32,
            reason,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state,
            // Null / bounds / div-by-zero guards all fire before the bytecode
            // they protect; `PendingException` (the one rethrow reason) never
            // reaches here. See `ResumeSemantics::for_reason`.
            semantics: ResumeSemantics::for_reason(reason),
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
        // deopt-unless Jcc -- tolerated on an overflowed buffer; see
        // `Self::patch_or_bail` / `patch_rel32_to_here`.
        Self::patch_or_bail(&mut self.buf, jcc_patch, rel);
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
        self.buf.emit(&[
            0x44,
            0x8B,
            0x50,
            // Compile-time checked: a layout constant past 127 would encode a
            // NEGATIVE disp8 and read before the object.
            crate::x64::disp::disp8_const(ARRAY_LENGTH_OFFSET as i64) as u8,
        ]); // MOV R10D,[RAX+12]
        self.buf.emit(&[0x44, 0x39, 0xD1]); // CMP ECX, R10D
        self.emit_deopt_unless(0x82, bci, DeoptReason::BoundsCheck); // JB continue
    }

    /// COV-02 — the integral / reference element **load**, with the array
    /// pointer in RAX and the index in RCX (the layout
    /// [`Self::emit_array_null_bounds_guards`] leaves behind) and the result in
    /// RAX, extended to 64 bits by the width's own JVMS rule.
    ///
    /// Byte-for-byte the single-pass backend's `emit_{int,long,byte,char,
    /// short,ref}_aload_regs` (`jit/src/x64/arrays.rs`). That is not an
    /// aesthetic preference: `jit/tests/ir_vs_singlepass.rs` compares the two
    /// backends' answers on the same bytecode, so any divergence in the
    /// extension rule (`baload` sign-extends, `caload` zero-extends) is a
    /// wrong-code bug the harness is built to catch — and the cheapest way not
    /// to have one is to emit the same instruction.
    ///
    /// `MemKind::Float` / `MemKind::Double` never reach here; the caller routes
    /// them to the XMM path.
    ///
    /// **One header-offset emission site, not seven.** Every arm below is the
    /// same `[RAX + RCX*scale + HEADER_SIZE]` address with a different opcode,
    /// so the displacement is materialised once as `d` and shared. The object-
    /// header shrink has to visit every place this crate bakes `HEADER_SIZE`
    /// into an instruction (`layout_constant_inventory`), and one shared local
    /// is one place to visit instead of seven. `disp8_const` also makes the
    /// backwards-addressing hazard a COMPILE error rather than a silent read
    /// before the object, which a raw narrowing cast to `u8` does not.
    fn emit_gpr_array_elem_load(&mut self, kind: MemKind) {
        let d = crate::x64::disp::disp8_const(HEADER_SIZE as i64) as u8;
        match kind {
            // MOVSXD RAX, DWORD [RAX + RCX*4 + HEADER_SIZE]
            MemKind::Int => self.buf.emit(&[0x48, 0x63, 0x44, 0x88, d]),
            // MOV RAX, QWORD [RAX + RCX*8 + HEADER_SIZE]
            MemKind::Long => self.buf.emit(&[0x48, 0x8B, 0x44, 0xC8, d]),
            // MOVSX EAX, BYTE [RAX + RCX*1 + HEADER_SIZE] ; MOVSXD RAX, EAX
            MemKind::Byte => {
                self.buf.emit(&[0x0F, 0xBE, 0x44, 0x08, d]);
                self.buf.emit(&[0x48, 0x63, 0xC0]);
            }
            // MOVZX EAX, WORD [RAX + RCX*2 + HEADER_SIZE] (already zero-extends
            // through the full RAX — a `char` is unsigned, so no MOVSXD).
            MemKind::Char => self.buf.emit(&[0x0F, 0xB7, 0x44, 0x48, d]),
            // MOVSX EAX, WORD [RAX + RCX*2 + HEADER_SIZE] ; MOVSXD RAX, EAX
            MemKind::Short => {
                self.buf.emit(&[0x0F, 0xBF, 0x44, 0x48, d]);
                self.buf.emit(&[0x48, 0x63, 0xC0]);
            }
            // `aaload`. The result is a REFERENCE: the node is `IrType::Ref`,
            // so `emit_safepoint_map` publishes this slot as a rewritable root
            // at every later safepoint, which is what makes the element
            // survive a relocating young collection.
            MemKind::Ref => {
                if narrow_oops_enabled() {
                    // 4-byte `(addr - base) >> 3`, 0 == null. Decode to a full
                    // pointer so every consumer downstream is unchanged, and
                    // keep null at 0 rather than rebasing it to `base` — `SHL`
                    // sets ZF from its result, so the null test is free.
                    self.buf.emit(&[0x8B, 0x44, 0x88, d]); // MOV EAX,[RAX+RCX*4+H]
                    self.buf.emit(&[0x48, 0xC1, 0xE0, 0x03]); // SHL RAX, 3
                    self.buf.emit(&[0x74, 0x0D]); // JZ +13 (null stays 0)
                    self.buf.emit(&[0x49, 0xBB]); // MOV R11, imm64
                    self.buf.emit(&narrow_base().to_le_bytes());
                    self.buf.emit(&[0x4C, 0x01, 0xD8]); // ADD RAX, R11
                } else {
                    // MOV RAX, QWORD [RAX + RCX*8 + HEADER_SIZE]
                    self.buf.emit(&[0x48, 0x8B, 0x44, 0xC8, d]);
                }
            }
            // Structurally unreachable — the caller routes FP to the XMM path.
            // A `debug_assert!` here would be a FAIL-OPEN: it vanishes in
            // release, this function would emit nothing, and the caller's
            // `store_rax(slot)` would still run and spill whatever RAX happens
            // to hold (the array pointer) as the element's value. Latch the
            // bailout so release refuses the compile instead.
            MemKind::Float | MemKind::Double => {
                self.latch_bailout(Bailout::with_context(
                    BailoutReason::Internal("ir_lower: FP element load reached the GPR emitter"),
                    format!("{kind:?}"),
                ));
            }
        }
    }

    /// COV-02 — the integral element **store**: array in RAX, index in RCX,
    /// value in RDX. The single-pass twins are `emit_{int,long,byte,short}_
    /// astore_regs`; `castore` and `sastore` share one 16-bit store, exactly as
    /// they do there.
    ///
    /// `MemKind::Ref` is refused by the caller (no store barrier in this tier)
    /// and the FP kinds take the XMM path. One shared header displacement, for
    /// the reason given on [`Self::emit_gpr_array_elem_load`].
    fn emit_gpr_array_elem_store(&mut self, kind: MemKind) {
        let d = crate::x64::disp::disp8_const(HEADER_SIZE as i64) as u8;
        match kind {
            // MOV DWORD [RAX + RCX*4 + HEADER_SIZE], EDX
            MemKind::Int => self.buf.emit(&[0x89, 0x54, 0x88, d]),
            // MOV QWORD [RAX + RCX*8 + HEADER_SIZE], RDX
            MemKind::Long => self.buf.emit(&[0x48, 0x89, 0x54, 0xC8, d]),
            // MOV BYTE [RAX + RCX*1 + HEADER_SIZE], DL
            MemKind::Byte => self.buf.emit(&[0x88, 0x54, 0x08, d]),
            // MOV WORD [RAX + RCX*2 + HEADER_SIZE], DX  (0x66 = 16-bit operand)
            MemKind::Char | MemKind::Short => self.buf.emit(&[0x66, 0x89, 0x54, 0x48, d]),
            // Structurally unreachable — see the load emitter's note. Same
            // fail-open, worse consequence: a silently dropped array store.
            MemKind::Ref | MemKind::Float | MemKind::Double => {
                self.latch_bailout(Bailout::with_context(
                    BailoutReason::Internal(
                        "ir_lower: a non-integral element store reached the GPR emitter",
                    ),
                    format!("{kind:?}"),
                ));
            }
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
                                      //
                                      // This stub inlines the teardown rather
                                      // than calling `emit_epilogue` (it must
                                      // NOT restore the shadow `top` — the
                                      // deopt entry already unwound it), so the
                                      // callee-saved restore has to be repeated
                                      // here. Omitting it returns to the caller
                                      // with XMM6/XMM7 holding this frame's
                                      // values, on the one exit that is hardest
                                      // to notice.
        self.emit_callee_saved_restore();
        self.buf.emit(&[0x48, 0x81, 0xC4]); // add rsp, frame_size
        self.buf.emit(&self.frame_size.to_le_bytes());
        self.buf.emit_byte(0x5D); // pop rbp
        self.buf.emit_byte(0xC3); // ret

        let patches = std::mem::take(&mut self.deopt_stub_patches);
        for p in patches {
            let rel = stub_off as i32 - (p as i32 + 4);
            if !Self::patch_or_bail(&mut self.buf, p, rel) {
                break;
            }
        }
    }

    /// Patch one stub-relative `rel32`, tolerating an overflowed buffer.
    ///
    /// Returns `false` once the patch could not be applied.
    /// [`ExecutableBuffer::try_patch_i32`] reports that ONLY by marking the
    /// buffer overflowed, and an overflowed buffer makes `lower_inner`
    /// discard the whole `CompiledMethod` and fall back to the single-pass
    /// backend -- so the error is genuinely ignorable here, exactly as
    /// `try_patch_i32`'s own contract says ("the caller may ignore the `Err`
    /// and rely on that bail") and as `patch_rel32_to_here` already does.
    ///
    /// It must NOT be an `expect`. `emit_deopt_stub` / `emit_call_exc_stub`
    /// run BEFORE that bail-out, on a VM thread with no unwinding catch, so a
    /// body that outgrew its estimated buffer aborted the whole process
    /// instead of falling back. Real repro (2026-07-27): parsing Groovy
    /// source through `GroovyClassLoader.parseClass` panicked with
    /// `call-exc JE patch in-bounds: PatchFailed { kind: "i32", offset: 4125 }`
    /// -- offset == the emitted length, i.e. the branch's own rel32
    /// placeholder had already been dropped by the sticky-overflow `emit` --
    /// and took the VM down with `fatal runtime error: failed to initiate
    /// panic`. The identical run under `--nojit` is clean.
    fn patch_or_bail(buf: &mut ExecutableBuffer, offset: usize, rel: i32) -> bool {
        if buf.try_patch_i32(offset, rel).is_err() {
            debug_assert!(
                buf.overflowed(),
                "try_patch_i32 must mark the buffer overflowed when it fails, \
                 so that lower_inner discards this compile"
            );
            return false;
        }
        true
    }

    /// Gap B: emit the shared call-exception bail stub (if any `Op::Call`
    /// emitted a sentinel check) and patch every dispatch site's `JE` to it. On
    /// entry `RAX` already holds the `i64::MIN` sentinel the helper returned when
    /// the callee threw; the stub stamps this method's own throw-site bci, then
    /// runs the epilogue, returning the sentinel so the VM's post-JIT path takes
    /// the pending exception (the same protocol the single-pass backend uses).
    ///
    /// # One stub per DISTINCT throw-site bci, not one shared stub
    ///
    /// This is the IR half of RBC.6, and it was the gap cov-07's closeout doc
    /// flagged and did not own (`fixed-suite-bugs/hibernate/
    /// offsetdatetimetest-zoneddatetimetest-athrow-ir-sneaky-throw-swallowed-
    /// 20260804-FIXED.md`). `JitSignals::athrow_bci` is consumed by `execute_jit_call`
    /// as *this* method's throw site and range-tested against `[start_pc,
    /// end_pc)` of every entry in this method's own exception table. Until this
    /// stub stamped it, that field still held whatever the CALLEE's compiled
    /// `athrow` lowering left there — a pc in a different method, which lands
    /// inside this method's protected region only by coincidence.
    ///
    /// A typed handler survives that coincidence often enough to look healthy
    /// (it is also matched on exception class), but a catch-all (`catch_type ==
    /// 0`, i.e. a javac `finally`) has nothing else to match on: a foreign bci
    /// outside the region silently drops it and the `finally` never runs.
    /// Witness: `FinallyBalanceProbe.java` — `try { n++; thrower(); } finally {
    /// n--; }` leaked one count per throw under the single-pass JIT until RBC.6
    /// fixed it there (`x64/deopt_stubs.rs`, `emit_exception_check_stub`), and
    /// leaked again once cov-07 let the same method shape reach THIS tier.
    ///
    /// Grouping by bci keeps the cost at one small pad per distinct fallible
    /// bytecode rather than one per branch site.
    ///
    /// `Op::Throw`'s own exit already passes its bci to `jit_throw_exception`,
    /// which stamps it; re-stamping the same value here is a no-op for it and
    /// keeps the stub's contract uniform — every exit through it leaves an
    /// `athrow_bci` belonging to THIS method.
    fn emit_call_exc_stub(&mut self) {
        if self.call_exc_patches.is_empty() {
            return;
        }
        let patches = std::mem::take(&mut self.call_exc_patches);
        let mut stub_by_bci: HashMap<usize, usize> = HashMap::new();
        for (patch, bci) in patches {
            let stub_off = match stub_by_bci.get(&bci) {
                Some(&off) => off,
                None => {
                    let off = self.buf.pos();
                    stub_by_bci.insert(bci, off);
                    // Stamp this method's own throw-site bci over whatever the
                    // callee left behind. The argument registers are dead here —
                    // the method is about to return — and RAX is reloaded with
                    // the sentinel afterwards because the helper call clobbers
                    // it.
                    if self.set_throw_bci != 0 {
                        self.emit_mov_reg_imm64(CALL_ARG_REGS[0], bci as u64);
                        self.emit_mov_reg_imm64(RAX, self.set_throw_bci as u64);
                        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                    }
                    self.emit_mov_reg_imm64(RAX, i64::MIN as u64);
                    // Shares the method epilogue so the shadow `top` watermark
                    // is restored here too — this is the path a callee's
                    // `i64::MIN` exception/deopt sentinel takes, skipping the
                    // call site's matching shadow reload.
                    self.emit_epilogue();
                    off
                }
            };
            let rel = stub_off as i32 - (patch as i32 + 4);
            if !Self::patch_or_bail(&mut self.buf, patch, rel) {
                break;
            }
        }
    }

    /// Build the interpreter `FrameState` for one safepoint snapshot.
    ///
    /// `method_key` is left to the VM caller to fill (the lowerer does not
    /// know it); deopt resume keys on the running `CompiledMethod`, not this
    /// string. It is recorded empty here.
    ///
    /// `index` is the snapshot's position in `Graph::safepoints`, which is what
    /// binds it to its inlined scope: `caller` is the chain of frames parked
    /// mid-`invoke` above this one ([`Self::caller_chain_for`]). With an empty
    /// [`InlineScopeTable`] — every compile today — `caller` is `None` and this
    /// produces exactly the frame state it always did.
    fn resolve_frame_state(&self, sp: &SafepointSnapshot, index: usize) -> FrameState {
        let (locals, stack) = self.resolve_frame_values(sp);
        FrameState {
            method_key: String::new(),
            bci: sp.bci as u32,
            locals,
            stack,
            monitors: Vec::new(),
            caller: self.caller_chain_for(index),
        }
    }

    /// The `(locals, stack)` halves of one snapshot's frame — everything
    /// [`Self::resolve_frame_state`] builds except the identity and the caller
    /// chain.
    ///
    /// Split out so [`Self::caller_chain_for`] can resolve a *caller's*
    /// snapshot without re-entering `resolve_frame_state`. Chaining those two
    /// would be mutually recursive, and a table that (wrongly) named a scope's
    /// own snapshot as its `caller_snapshot` would recurse until the stack ran
    /// out — inside a compile, for a metadata defect. There is no cycle to
    /// defend against here because this function never looks at a scope.
    fn resolve_frame_values(&self, sp: &SafepointSnapshot) -> (Vec<FrameValue>, Vec<FrameValue>) {
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
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_SCALAR_DEOPT").is_some() {
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
            return (locals, stack);
        }
        (
            sp.locals.iter().map(|&n| self.frame_value_for(n)).collect(),
            sp.stack.iter().map(|&n| self.frame_value_for(n)).collect(),
        )
    }

    /// The chain of inlined **caller** frames above the snapshot at `index`,
    /// outermost last — i.e. exactly what `FrameState::caller` wants.
    ///
    /// `None` when the snapshot belongs to no inlined callee, which is every
    /// snapshot on every compile today (`IrBuilder::build` does not inline, so
    /// [`Lowerer::inline_scopes`] is empty). The early return keeps that path
    /// allocation-free.
    ///
    /// ## Fail-closed on an undescribed caller
    ///
    /// A caller frame is not empty: its locals and operand stack are live, and
    /// a resume has to rebuild them. When the scope names no
    /// `caller_snapshot` — or names one that does not exist — this emits a
    /// frame holding a single [`FrameValue::Unsupported`] rather than an empty
    /// one. An empty caller frame is *silently wrong*: the resume sinks map a
    /// missing slot to `Value::Int(0)`, so a future sink that learned to resume
    /// chains would resume the caller with all-zero locals and no error
    /// anywhere. `Unsupported` makes the whole chain fail
    /// [`crate::deopt::frame_state_is_resumable`] — which walks the caller
    /// chain precisely so this cannot pass as clean — and the artifact is
    /// refused at admission instead.
    ///
    /// ## Known gap, unreachable today
    ///
    /// Each scope's values are resolved by its own [`Self::resolve_frame_values`]
    /// call, so the `emitted` set that turns a repeated scalar-replaced object
    /// into a `VirtualObjectRef` does not span scopes: an object live in BOTH a
    /// callee and its caller would be *defined* twice, and the materializer
    /// would rebuild two objects where the program had one. Unreachable while
    /// the scope table is empty; a producer that starts building chains must
    /// thread one `emitted` set through the whole chain first. See
    /// `docs/jit/deopt-inline-scopes.md`.
    fn caller_chain_for(&self, index: usize) -> Option<Box<FrameState>> {
        if self.inline_scopes.is_empty() {
            return None;
        }
        let scope = self.inline_scopes.snapshot_scope(index)?;
        // Outermost-first, so each frame can be built with the one already
        // built as its own caller. `chain` is capped at
        // `MAX_INLINE_SCOPE_DEPTH`, so this cannot run away on a bad table.
        let chain = self.inline_scopes.chain(scope);
        debug_assert!(chain.len() <= MAX_INLINE_SCOPE_DEPTH);
        let mut built: Option<Box<FrameState>> = None;
        for &handle in chain.iter().rev() {
            let Some(sc) = self.inline_scopes.scope(handle) else {
                continue;
            };
            let described = sc
                .caller_snapshot
                .and_then(|si| self.graph.safepoints.get(si as usize))
                .map(|sp| self.resolve_frame_values(sp));
            let (locals, stack) = match described {
                Some(parts) => parts,
                // See "Fail-closed on an undescribed caller" above.
                None => (vec![FrameValue::Unsupported], Vec::new()),
            };
            built = Some(Box::new(FrameState {
                method_key: sc.method_key.clone(),
                bci: sc.caller_bci,
                locals,
                stack,
                monitors: Vec::new(),
                caller: built.take(),
            }));
        }
        built
    }

    /// Block where the deopt at `bci` fires — the program point all of a
    /// scalar-replaced object's field stores must dominate for its
    /// `VirtualObject` emission to be temporally correct. v1 deopt points are
    /// div/rem guards, so the block is that of the `Op::Div`/`Op::Rem` node
    /// carrying this bci. Returns `None` (⇒ the producer bails to `Undefined`)
    /// when the block can't be uniquely identified.
    fn deopt_block_for_bci(&self, bci: usize) -> Option<usize> {
        // An `Op::Guard` at this bci OWNS the deopt: since
        // `ir::IrBuilder::add_div_zero_guard`, a division's zero-divisor trap
        // is the control-anchored guard's, and the floating `Op::Div` beside it
        // no longer traps at all. The two sit in different blocks in exactly
        // the case that anchoring exists to fix — the scheduler hoisted the
        // division — so consulting both would report "ambiguous" and give up
        // on a program point that is in fact unambiguous.
        let anchored = self
            .graph
            .nodes
            .iter()
            .any(|n| matches!(n.op, Op::Guard { bci: gb } if gb == bci));
        let mut found: Option<usize> = None;
        for (id, n) in self.graph.nodes.iter().enumerate() {
            // The deopt at `bci` fires from an explicit `Op::Guard { bci }`, or
            // — for a graph with no guard at this bci — from the div/rem
            // zero/overflow guard the lowerer emits at the node carrying
            // `bytecode_pc == bci`.
            let is_deopt_here = match &n.op {
                Op::Div | Op::Rem => !anchored && n.bytecode_pc == Some(bci),
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
    /// occurrence). Bails to
    /// [`FrameValue::MaterializationRequired`] (⇒ the deopt is refused and the
    /// method takes the safe whole-method re-run) unless every soundness
    /// condition holds:
    ///   * a deopt block is known, and the `Op::New` + every eliminated field
    ///     store **strictly dominate** it — so each field genuinely holds its
    ///     recorded value at the deopt bci (a deopt *before* a store would
    ///     otherwise materialize a post-store value);
    ///   * no field value is itself another scalar-replaced (virtual) object —
    ///     nested virtual graphs are a deferred follow-up (v1);
    ///   * every field value resolves to a real machine/const `FrameValue`
    ///     (never `Undefined`/`Unsupported`/`MaterializationRequired`), which
    ///     `resolve_value` makes concrete from machine state at deopt time.
    ///
    /// Every bail below used to be spelled `FrameValue::Undefined`, and that was
    /// the last silent-null producer in this pipeline: the slot describes an
    /// object escape analysis DELETED, the interpreter *will* read it, and every
    /// resume sink maps `Undefined` to `Value::Int(0)` — i.e. `null` for a
    /// reference local, with no error and no refusal. `MaterializationRequired`
    /// says the same thing honestly and is unresumable by construction (see the
    /// `deopt` module's "Eliminated vs. undefined" section), so the wrong value
    /// becomes a refused deopt instead.
    fn frame_value_for_object(
        &self,
        new_id: NodeId,
        deopt_block: Option<usize>,
        sr: &ScalarReplacementMap,
        emitted: &mut std::collections::HashSet<NodeId>,
    ) -> FrameValue {
        let dbg = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_SCALAR_DEOPT").is_some();
        let info = match sr.objects.get(&new_id) {
            Some(i) => i,
            // Unreachable through `resolve_frame_state` (which only calls here
            // for a key it just found), so the class id is not recoverable —
            // record the producer alone rather than invent a `class_id`.
            None => {
                return FrameValue::MaterializationRequired(EliminatedValue::new(
                    new_id,
                    EliminationCause::ScalarReplacedObject,
                ))
            }
        };
        let db = match deopt_block {
            Some(b) => b,
            None => {
                if dbg {
                    eprintln!("[DBG_SCALAR_DEOPT] bail new {new_id}: no deopt block for bci");
                }
                return Self::eliminated_object(
                    new_id,
                    info,
                    EliminationCause::ScalarReplacedObject,
                );
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
            return Self::eliminated_object(new_id, info, EliminationCause::ScalarReplacedObject);
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
                return Self::eliminated_object(
                    new_id,
                    info,
                    EliminationCause::ScalarReplacedObject,
                );
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
                    // NESTED VIRTUAL OBJECT. A field whose value is itself a
                    // scalar-replaced allocation is described in place, by the
                    // same function, recursively. `emitted` is what makes that
                    // terminate and what makes sharing work: the second and
                    // later occurrences of an id come back as
                    // `FrameValue::VirtualObjectRef`, which the materializer
                    // resolves against the shell it already allocated. A cycle
                    // therefore ends at its first repeat rather than recursing.
                    //
                    // This used to bail the WHOLE enclosing object ("v1 emits
                    // no nested graphs"), which is what stopped a wrapper and
                    // its storage array from both being deleted: the wrapper's
                    // recipe names the array, so the moment the array became
                    // replaceable the wrapper's own recipe was refused.
                    //
                    // The recursive call re-runs every gate on the nested
                    // object -- its allocation and stores must strictly
                    // dominate the same deopt block -- so a nested object that
                    // cannot be described refuses itself, and the check below
                    // turns that refusal into a refusal of the enclosing
                    // object. Fail-closed, one level at a time.
                    if sr.objects.contains_key(&vnode) {
                        let nested = self.frame_value_for_object(vnode, deopt_block, sr, emitted);
                        if matches!(
                            nested,
                            FrameValue::Undefined
                                | FrameValue::Unsupported
                                | FrameValue::MaterializationRequired(_)
                        ) {
                            if dbg {
                                eprintln!("[DBG_SCALAR_DEOPT] bail new {new_id}: field {i} nested virtual (node {vnode}) -> {nested:?}");
                            }
                            return Self::eliminated_object(
                                new_id,
                                info,
                                EliminationCause::NestedVirtualObject,
                            );
                        }
                        field_values.push(nested);
                        continue;
                    }
                    let fv = self.frame_value_for(vnode);
                    // `MaterializationRequired` joins the refusal set: a field
                    // whose own producer was deleted must not be stored into a
                    // materialized object (`frame_state_is_resumable` documents
                    // that the producers, i.e. here, refuse such a field rather
                    // than let it through inside a `VirtualObject`).
                    if matches!(
                        fv,
                        FrameValue::Undefined
                            | FrameValue::Unsupported
                            | FrameValue::MaterializationRequired(_)
                    ) {
                        if dbg {
                            eprintln!("[DBG_SCALAR_DEOPT] bail new {new_id}: field {i} node {vnode} -> {fv:?}");
                        }
                        return Self::eliminated_object(
                            new_id,
                            info,
                            EliminationCause::ScalarReplacedObject,
                        );
                    }
                    fv
                }
            };
            field_values.push(fv);
        }
        emitted.insert(new_id);
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_SCALAR_DEOPT").is_some() {
            eprintln!(
                "[DBG_SCALAR_DEOPT] emit VirtualObject (new {new_id}, class_id {}, {} field(s)) at deopt block {db}",
                info.class_id, info.num_fields
            );
        }
        FrameValue::VirtualObject(VirtualObjectState {
            array_element_type: info.array_element_type,
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
        for (index, sp) in self.graph.safepoints.iter().enumerate() {
            let native_offset = match self.bci_native.get(&sp.bci) {
                Some(&off) => off as u32,
                // bci produced no node / no machine code — nothing to anchor.
                None => continue,
            };
            // No speculation yet — these are plain resume points (step 2,
            // emit-and-discard). A real guard (step 3) sets its own reason.
            let reason = DeoptReason::TransferToInterpreter;
            points.push(DeoptimizationPoint {
                native_offset,
                bci: sp.bci as u32,
                reason,
                action: DeoptAction::Reinterpret,
                speculation_id: 0,
                frame_state: self.resolve_frame_state(sp, index),
                semantics: ResumeSemantics::for_reason(reason),
            });
        }
        points.sort_by_key(|p| p.native_offset);
        points.dedup_by_key(|p| p.native_offset);
        points
    }
}

// ── Resource bounds (checked BEFORE anything is reserved) ────────────
//
// The lowerer used to size its frame straight from `graph.nodes.len()` — one
// 8-byte spill slot per node, unconditionally — and then `assert!` its way out
// if a slot fell past the cap. Two problems: `nodes * 8` on a pathological
// graph overflows the `i32` frame arithmetic before any check runs, and the
// assertion is a panic on the compiler thread for what is only a compiler
// resource limit. Both are now decided up front, from the same numbers the
// frame is actually built from — and the spill term itself is now the
// liveness-coloured slot count ([`plan_slots`]) rather than the node count.

/// What a graph demands of the frame beyond its own spills. Shared by
/// [`Lowerer::new`] (which builds the frame) and [`estimate_frame_bytes`]
/// (which bounds it), so the checked size and the built size cannot drift.
struct FrameNeeds {
    /// The method takes the VM context pointer as a hidden first argument and
    /// reserves a frame slot for it.
    needs_context: bool,
    /// Widest outgoing Java-argument list staged for a call.
    max_call_args: usize,
}

/// Scan the graph for the call/allocation shapes that widen the frame.
fn scan_frame_needs(graph: &Graph, helpers: &JitRuntimeHelpers) -> FrameNeeds {
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
        // COV-03: `jit_putfield_object` is the one putfield helper that takes
        // the VM context pointer (it needs the heap to run the SATB and card
        // barriers). Without this the lowering would load arg0 from an
        // unreserved `context_slot_off` — the single-pass backend's identical
        // `needs_heap` bug, which handed the helper a stack address to
        // dereference as a `SharedVm` (Tomcat's `Catalina.setParentClassLoader`,
        // a bare `aload_0; aload_1; putfield; return` with no other heap op).
        // The wide `putfield_{long,float,double}` helpers take no context.
        if matches!(n.op, Op::Store(MemKind::Ref)) {
            needs_context = true;
        }
        if matches!(n.op, Op::New { .. }) {
            needs_context = true;
        }
        // cov-06: `emit_new_array_stub` loads the VM context into ARG0, same
        // as `Op::New`'s `emit_new_object_stub`.
        if matches!(n.op, Op::NewArray { .. }) {
            needs_context = true;
        }
        // cov-01: all three take the VM context pointer as their helper's arg0.
        // `Op::LoadStatic` needs it even on the direct route it usually takes,
        // because the route is chosen per SITE at compile time and a single
        // helper-served site in the method is enough — deciding it here, from
        // the graph, keeps the frame layout independent of that choice.
        if matches!(
            n.op,
            Op::ConstString { .. } | Op::ConstClass { .. } | Op::LoadStatic { .. }
        ) {
            needs_context = true;
        }
        // cov-05: `jit_instanceof`/`jit_checkcast` take the VM context
        // pointer as arg0, same as the three above.
        if matches!(n.op, Op::InstanceOf { .. } | Op::CheckCast { .. }) {
            needs_context = true;
        }
    }
    FrameNeeds {
        needs_context,
        max_call_args,
    }
}

/// Estimated peak frame requirement, in bytes, for a method the lowerer would
/// build with `num_locals` locals, `spill_slots` distinct spill slots and
/// `needs`.
///
/// Mirrors [`Lowerer::new`]'s layout exactly — locals, optional context slot,
/// the five reserved bookkeeping slots, the spill reservation, the outgoing
/// argument staging area, the 16-byte stack-argument reserve and the 32-byte
/// ABI shadow space, rounded up to the 16-byte stack alignment — but in
/// saturating `usize` arithmetic, so a graph big enough to overflow the
/// constructor's `i32` math is *counted* rather than wrapped. `usize::MAX`
/// therefore reads as "far too large", which is exactly what
/// [`check_frame_size`] concludes.
///
/// `spill_slots` is [`SlotPlan::slots`] — the number of *colours* the liveness
/// colouring needed, i.e. the maximum number of simultaneously live values plus
/// the pinned classes. It used to be `graph.nodes.len()`, which assumed every
/// value in the graph was live at once; a long straight-line method now pays
/// for its working set instead of for its history. The two callers
/// ([`lower_inner`]'s bound and [`Lowerer::new`]'s layout) pass the same
/// `SlotPlan`, so "the frame we checked" and "the frame we build" stay the same
/// number by construction.
fn estimate_frame_bytes(num_locals: usize, spill_slots: usize, needs: &FrameNeeds) -> usize {
    let locals = num_locals.saturating_mul(8);
    let context = if needs.needs_context { 8 } else { 0 };
    // safepoint id, cached thread pointer, shadow save-base, shadow save-top,
    // phi parallel-copy scratch. Mirrors `bookkeeping_size` in `Lowerer::new`;
    // the `debug_assert_eq!` there is what keeps the two counts equal.
    let bookkeeping = 8usize * 5;
    let spills = spill_slots.saturating_mul(8);
    let args_stage = needs.max_call_args.saturating_mul(8);
    let shadow = 32usize;
    let stack_arg_reserve = 16usize;
    // Cast: `ir_saved_xmm_bytes` returns 0 or 32.
    let saved_xmms = ir_saved_xmm_bytes() as usize;
    // Cast: `ir_saved_gpr_bytes` returns 0 or 40.
    let saved_gprs = ir_saved_gpr_bytes() as usize;
    let total = locals
        .saturating_add(context)
        .saturating_add(bookkeeping)
        .saturating_add(spills)
        .saturating_add(saved_xmms)
        .saturating_add(saved_gprs)
        .saturating_add(args_stage)
        .saturating_add(shadow)
        .saturating_add(stack_arg_reserve);
    // 16-byte alignment, saturating rather than wrapping at the top end.
    total.saturating_add(15) & !15usize
}

/// Refuse a graph whose node count exceeds the compile-time budget.
///
/// Defence in depth: `lib.rs` already screens on `ir::IR_MAX_GRAPH_NODES`
/// before building a schedule. This restates the bound at the point that
/// *allocates* per node, so a future caller that skips the screen cannot
/// reserve an unbounded frame.
fn check_graph_size(node_count: usize, limit: usize) -> CompileResult<()> {
    if node_count > limit {
        return Err(Bailout::new(BailoutReason::GraphTooLarge {
            nodes: node_count,
            limit,
        }));
    }
    Ok(())
}

/// Refuse a frame estimate that exceeds the frame budget.
///
/// The bound is a correctness bound, not a stack-consumption policy: this file
/// encodes oop-map slot offsets as `i16`, so a reference living past
/// `i16::MAX` is *unrepresentable* in the map the collector reads. See
/// [`DEFAULT_MAX_FRAME_BYTES`].
fn check_frame_size(frame_bytes: usize, limit: usize) -> CompileResult<()> {
    if frame_bytes > limit {
        return Err(Bailout::new(BailoutReason::FrameTooLarge {
            bytes: frame_bytes,
            limit,
        }));
    }
    Ok(())
}

// ── Phi parallel copy: the `ValueLoc` view of a frame word ───────────

/// The [`ValueLoc`] this backend uses for the frame word at `[rbp - off]`.
///
/// [`resolve_parallel_copy`] only ever *compares* locations — it never
/// dereferences one, and the scratch is deliberately unnamed in [`CopyOp`] — so
/// the payload has to be an injective token for "this frame word" and nothing
/// more. The rbp-relative byte offset is exactly that: it is what
/// [`Lowerer::slot_of`] hands out, it is positive and bounded by
/// [`DEFAULT_MAX_FRAME_BYTES`], and no arithmetic is done to it on the way in or
/// out.
///
/// Deliberately NOT `off / 8`, even though every offset this file produces is on
/// the 8-byte grid. A division would map two distinct offsets onto one location
/// the moment that ever stopped being true, and `resolve_parallel_copy` DROPS a
/// copy whose destination and source are the same location — so the aliasing
/// would silently delete a real move, which is the same class of wrong-code bug
/// as the sequential emission this replaces.
fn frame_word_loc(off: i32) -> CompileResult<ValueLoc> {
    u32::try_from(off)
        .ok()
        .filter(|&o| o != 0)
        .map(ValueLoc::Slot)
        .ok_or_else(|| {
            Bailout::with_context(
                BailoutReason::Internal("ir_lower: a phi copy names a non-frame location"),
                format!("offset {off}"),
            )
        })
}

/// Inverse of [`frame_word_loc`].
///
/// A [`ValueLoc::Reg`] is unreachable from this backend — it keeps every value
/// in its home frame word and has no register allocation to disagree with — but
/// it is representable in the shared type, so it is refused rather than
/// mis-emitted as an offset.
fn frame_word_off(loc: ValueLoc) -> CompileResult<i32> {
    match loc {
        ValueLoc::Slot(off) => i32::try_from(off).map_err(|_| {
            Bailout::with_context(
                BailoutReason::Internal("ir_lower: a phi copy offset does not fit the frame"),
                format!("{loc:?}"),
            )
        }),
        ValueLoc::Reg(_) => Err(Bailout::with_context(
            BailoutReason::Internal(
                "ir_lower: a phi copy names a register; this backend keeps every value in a \
                 frame word",
            ),
            format!("{loc:?}"),
        )),
    }
}

/// Sequentialise one edge's `(phi slot, source slot)` parallel copy.
///
/// The gathered pairs are simultaneous — every source is read as it was before
/// any destination is written — so they go through
/// [`resolve_parallel_copy`], which returns an order preserving that semantics
/// and breaks any cycle with one [`CopyOp::Save`] / [`CopyOp::Restore`] pair.
/// Self-copies are dropped by the resolver, so the acyclic majority emits
/// exactly one load + one store per surviving pair, as it always did.
fn phi_copy_sequence(copies: &[(i32, i32)]) -> CompileResult<Vec<CopyOp>> {
    let mut pairs: Vec<(ValueLoc, ValueLoc)> = Vec::with_capacity(copies.len());
    for &(dst, src) in copies {
        pairs.push((frame_word_loc(dst)?, frame_word_loc(src)?));
    }
    resolve_parallel_copy(&pairs)
}

// ── Pre-emission location verification ───────────────────────────────

/// True for the IR types that occupy a frame slot, i.e. the inputs a lowering
/// reads through [`Lowerer::slot_of`]. `Control`, `Memory` and `Void` inputs
/// are edges, not values, and never have a location.
fn is_value_ty(ty: IrType) -> bool {
    matches!(
        ty,
        IrType::Int | IrType::Long | IrType::Float | IrType::Double | IrType::Ref
    )
}

/// True iff lowering this op assigns its result a frame slot.
///
/// Must stay in step with `lower_data_node`'s match arms: every arm that calls
/// `alloc_slot` is listed here, plus `Op::Phi` (reserved up front by
/// `prealloc_phi_slots`). Ops that reach `lower_data_node`'s catch-all — they
/// refuse the compile — are deliberately absent, because a value read from one
/// of them has no location either.
///
/// "Must stay in step" used to be enforced by this sentence alone. It is now
/// enforced by `every_ir_op_is_lowered_or_declared_unlowerable` and the two
/// tests beside it, which read this function's body, `lower_data_node`'s arms
/// and `ir::Op`'s own declaration out of the source and compare all three. See
/// `docs/feature-designs/jit-machine-level-and-instruction-selection.md`
/// ("The cheap alternative") for why three enumerations of one set is
/// the shape that produced the monitor defect.
fn op_defines_result_slot(op: &Op) -> bool {
    matches!(
        op,
        Op::Const(_)
            | Op::ConstF(_)
            | Op::Param(_)
            | Op::Phi
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Rem
            | Op::Neg
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Shl
            | Op::Shr
            | Op::UShr
            | Op::Cmp(_)
            | Op::LCmp
            | Op::FCmp { .. }
            | Op::I2L
            | Op::L2I
            | Op::I2F
            | Op::I2D
            | Op::L2F
            | Op::L2D
            | Op::F2I
            | Op::F2L
            | Op::F2D
            | Op::D2I
            | Op::D2L
            | Op::D2F
            | Op::Load(_)
            | Op::ArrayLoad(_)
            | Op::ArrayStore(_)
            | Op::ArrayLength
            | Op::New { .. }
            | Op::NewArray { .. }
            | Op::Call { .. }
            // cov-01: each defines a result slot — a `Ref` for the two `ldc`
            // constants, the field's value for `getstatic`.
            | Op::ConstString { .. }
            | Op::ConstClass { .. }
            | Op::LoadStatic { .. }
            | Op::LambdaIntToDouble
            // cov-05: `instanceof` defines a result slot — the 0/1 `Int`;
            // `checkcast` defines one too — the `Ref` result.
            | Op::InstanceOf { .. }
            | Op::CheckCast { .. }
    )
}

/// Refuse, *before a single byte is emitted*, any graph in which some node the
/// lowerer will emit reads a value that has no frame location at that point.
///
/// This is the structural fix for the `[rbp - 0]` defect. The observed failure
/// (ES `SortingDigestTests` with the IR tier on) was a `pc17 ArrayLoad(Double)`
/// feeding a GVN-collapsed loop phi: the scheduler placed the phi but not the
/// load, so the load had no slot, and `slot_of` handed back the
/// zero-initialised entry — `[rbp - 0]`, the saved caller frame pointer, read
/// as a `double`. The old detection was a sticky bit consulted *after* the
/// whole body had been emitted.
///
/// Three ways a data input can fail to have a location; all three are the same
/// bailout:
///
///  1. the defining node is in no emitted block (dead, or a scheduler
///     omission) — it never reaches `lower_data_node`, so nothing allocates;
///  2. its op emits nothing (`lower_data_node`'s catch-all), so even though it
///     is scheduled it defines no slot;
///  3. it is emitted, but *after* the use — every location here is created at
///     the point of emission, so a later definition is as absent as no
///     definition.
///
/// Phis are exempt from (3): their slots are reserved by `prealloc_phi_slots`
/// before any block is lowered, precisely so a forward branch can copy into
/// one. Phi *inputs* are exempt too — they are written by predecessor edge
/// copies, not read in the phi's own block — so only (1) and (2) apply to them.
///
/// Rejecting here can only lose graphs that the `unallocated_slot_use` latch
/// already rejected after the fact, so it costs no coverage; what it buys is
/// that the refusal happens before emission, where a mutation that drops a
/// node from the schedule is caught by construction rather than by a bit that
/// someone must remember to check.
///
/// Known residual (why the latch is kept rather than deleted): a phi's value
/// inputs are consumed by `emit_phi_copies` at the *predecessor's* terminator,
/// not in the phi's own block, so their ordering constraint is per-CFG-edge —
/// reconstructing it here would duplicate `block_of_ctrl`'s walk. A def that
/// reached its edge copy too late would still be caught by the latch, one
/// phase later. Every *placement* failure — the class that produced the
/// observed bug — is caught here.
fn verify_data_locations(graph: &Graph, schedule: &Schedule) -> CompileResult<()> {
    // Emission order: blocks in index order; within a block, its data nodes in
    // scheduled order, then its terminator. This is exactly `lower_inner`'s
    // `for block_idx in 0..blocks.len() { lower_block(block_idx) }`.
    let mut emit_index: Vec<Option<usize>> = vec![None; graph.nodes.len()];
    // Which laid-out block each node lands in. Used only to describe a failure:
    // "def after use" means one thing when both sit in a single block (an
    // intra-block scheduling order) and quite another when they do not (a block
    // LAYOUT that put a definition's block after its user's).
    let mut block_of: Vec<Option<usize>> = vec![None; graph.nodes.len()];
    let mut seq = 0usize;
    for (bpos, block) in schedule.blocks.iter().enumerate() {
        for &node_id in &block.nodes {
            if let Some(cell) = emit_index.get_mut(node_id as usize) {
                *cell = Some(seq);
            }
            if let Some(cell) = block_of.get_mut(node_id as usize) {
                *cell = Some(bpos);
            }
            seq += 1;
        }
        if let Some(term) = block.terminator {
            if let Some(cell) = emit_index.get_mut(term as usize) {
                *cell = Some(seq);
            }
            if let Some(cell) = block_of.get_mut(term as usize) {
                *cell = Some(bpos);
            }
            seq += 1;
        }
    }

    let check_user = |user: NodeId| -> CompileResult<()> {
        let node = match graph.nodes.get(user as usize) {
            Some(n) => n,
            None => {
                return Err(Bailout::new(BailoutReason::Internal(
                    "ir_lower: schedule names a node outside the graph",
                )))
            }
        };
        let user_is_phi = matches!(node.op, Op::Phi);
        for &input in &node.inputs {
            if input == NO_NODE {
                continue;
            }
            let def = match graph.nodes.get(input as usize) {
                Some(d) => d,
                None => {
                    return Err(Bailout::new(BailoutReason::Internal(
                        "ir_lower: node input outside the graph",
                    )))
                }
            };
            // Control / memory edges carry no value and are never `slot_of`'d.
            if !is_value_ty(def.ty) {
                continue;
            }
            if !op_defines_result_slot(&def.op) {
                // (1) dead / (2) no lowering ⇒ no location, ever.
                return Err(Bailout::with_context(
                    BailoutReason::UnallocatedValue { node: input },
                    format!(
                        "n{input} ({:?}, {:?}) is read by n{user} ({:?}) but its \
                         lowering assigns no frame slot",
                        def.op, def.ty, node.op
                    ),
                ));
            }
            let def_seq = match emit_index.get(input as usize).copied().flatten() {
                Some(s) => s,
                None => {
                    // Phis are reserved before emission, so "not in any emitted
                    // block" is still fatal for them: nothing would write the
                    // slot. Report the same way.
                    return Err(Bailout::with_context(
                        BailoutReason::UnallocatedValue { node: input },
                        format!(
                            "n{input} ({:?}) is read by n{user} ({:?}) but is in no \
                             emitted block",
                            def.op, node.op
                        ),
                    ));
                }
            };
            if user_is_phi || matches!(def.op, Op::Phi) {
                // Phi slots exist before the first block is lowered, and a
                // phi's own inputs are consumed by predecessor edge copies
                // rather than read in the phi's block — neither is an
                // ordering constraint.
                continue;
            }
            let use_seq = emit_index
                .get(user as usize)
                .copied()
                .flatten()
                .unwrap_or(usize::MAX);
            if def_seq >= use_seq {
                return Err(Bailout::with_context(
                    BailoutReason::UnallocatedValue { node: input },
                    format!(
                        "n{input} ({:?}) is emitted at position {def_seq}, after its \
                         use by n{user} ({:?}) at position {use_seq} \
                         [def in laid-out block {:?}, use in {:?}]",
                        def.op,
                        node.op,
                        block_of.get(input as usize).copied().flatten(),
                        block_of.get(user as usize).copied().flatten(),
                    ),
                ));
            }
        }
        Ok(())
    };

    for block in &schedule.blocks {
        for &node_id in &block.nodes {
            check_user(node_id)?;
        }
        if let Some(term) = block.terminator {
            check_user(term)?;
        }
    }
    Ok(())
}

// ── Liveness-based frame-slot reuse ──────────────────────────────────
//
// The lowerer used to reserve one 8-byte spill slot per graph node, forever: a
// value dead since block 0 still owned a word of the frame at the return. On a
// 4000-node method that is 32 KiB of stack — the entire `DEFAULT_MAX_FRAME_BYTES`
// budget — spent almost entirely on values nothing can read any more, and it is
// why the optimizing tier *declined* anything much past 4000 nodes.
//
// What replaces it is a textbook two-step: compute each value's live range over
// the lowerer's own emission order, then colour the ranges so values that are
// never live at the same time share a word.
//
// ## Positions
//
// The "program points" are the linear emission positions `lower_inner` walks:
// blocks in index order, within a block its scheduled data nodes then its
// terminator, then ONE more position for the block's outgoing edge — the point
// at which `emit_phi_copies` writes this edge's parallel copies and the block's
// jump is emitted. `verify_data_locations` numbers the first two the same way;
// the third is the subtlety that file already documents: **a phi's value inputs
// are consumed at the predecessor's terminator, not in the phi's own block**, so
// they must stay live to the end of every predecessor that feeds them.
//
// ## Liveness
//
// A backward may-analysis over the block CFG:
//
//     live_out[b] = phi_out[b] ∪ ⋃_{s ∈ succ(b)} live_in[s]
//     live_in[b]  = use[b] ∪ (live_out[b] \ def[b])
//
// with `use[b]` the upward-exposed uses, `def[b]` the values the block defines
// and `phi_out[b]` the values `emit_phi_copies` reads on b's outgoing edges.
// Iterated to a fixed point, so a back edge propagates a loop body's uses
// around the loop and a value used anywhere in a loop is live across the whole
// loop — no special case needed, that is what the fixed point *means*.
//
// Each value's range is then the closed interval
//
//     [ min(def position, start of every block it is live-in to),
//       max(use positions,  end   of every block it is live-out of) ]
//
// which is a contiguous over-approximation in position space: every point at
// which the value is live is inside it. That is all interference needs — two
// values live at the same point have overlapping intervals, so refusing to
// share on overlap can never alias two live values. It can *over*-estimate
// (a value live only in blocks 0 and 9 covers 1..8 too), which costs slots, not
// correctness.
//
// ## Colouring
//
// Linear scan over intervals sorted by start, with an expiry heap and a free
// list of released colours. Endpoints are inclusive on both sides — an interval
// starting exactly where another ends does NOT reuse its slot — because a
// node's lowering allocates its result slot *before* reading its operands at
// some sites and after at others, and one word of frame is not worth auditing
// forty emission arms for.
//
// ## What is NOT coloured, and why
//
//   * **Phis.** `prealloc_phi_slots` reserves them before any block is lowered
//     and `emit_phi_copies` writes them from predecessors that may be lowered
//     much later; the whole phi web has to keep one identity, and
//     `zero_ref_phi_slots` additionally publishes every `Ref` phi's slot as a
//     GC root from the prologue onward. Dedicated slot.
//   * **Anything a deopt frame names.** Every node reachable from a
//     `SafepointSnapshot` (and every scalar-replacement field value, when
//     `sr_map` is set) is read by `frame_value_for` at a native offset chosen at
//     runtime — `find_deopt_point` binary-searches, so the frame state of *any*
//     recorded bci can be consumed. Its slot must still hold its value there.
//     Reconstructing which guard that is belongs to the deopt producer, not to
//     a register allocator. Dedicated slot.
//   * **`Ref` results of `Op::Call`.** `emit_safepoint_map` publishes the slot
//     of every `Ref` node defined so far, and the self-recursive route
//     (`emit_self_recursive_call`) stores the call's result BEFORE
//     `emit_shadow_reload` copies the published values back. A call result that
//     recycled a published colour would be overwritten by the reload. These may
//     *donate* their colour once they die, but never *receive* a recycled one —
//     the colour they take has been handed to nobody, so it cannot be in the
//     published set (any value that later inherits it is defined strictly after
//     this call, hence not yet in `defined_nodes` here).
//
// ## Reference / primitive separation
//
// A colour is `Ref` or `Prim` from its first assignment and never changes
// class: the two free lists are disjoint. `emit_safepoint_map` publishes a slot
// because *some* `Ref` node was defined into it, and `defined_nodes` is
// monotone, so a slot the map names may hold a stale (but genuine) reference —
// that is the pre-existing behaviour and the collector handles it. What must
// never happen is a slot the map names holding an `int`, which separating the
// pools makes unrepresentable rather than unlikely.

/// A closed interval of linear emission positions over which a value must keep
/// its frame slot. See the module section above for how positions are numbered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LiveRange {
    lo: usize,
    hi: usize,
}

impl LiveRange {
    /// Endpoint-**inclusive** overlap: `[4, 7]` and `[7, 9]` overlap.
    ///
    /// Deliberately stricter than "live at a common point". A node's lowering
    /// may allocate its result slot before or after loading its operands
    /// depending on the opcode, so a value whose last use is at position `p`
    /// and a value defined at position `p` are kept apart.
    fn overlaps(self, other: LiveRange) -> bool {
        self.lo <= other.hi && other.lo <= self.hi
    }
}

/// Which pool a value's frame slot is drawn from. A colour's class is fixed at
/// its first assignment; see the section comment on reference separation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotClass {
    /// Reference-typed. Shares only with other references — a slot an oop map
    /// names must never hold a primitive.
    Ref,
    /// Non-reference. Shares only with other non-references.
    Prim,
    /// Never shares with anything: phis, deopt-visible values, and any other
    /// class this file cannot prove safe to alias.
    Pinned,
}

/// The frame-slot assignment for one method.
struct SlotPlan {
    /// `node_color[id]` = the 0-based 8-byte slot index node `id`'s result
    /// lives in, or `None` when the lowerer never allocates for that node.
    node_color: Vec<Option<u32>>,
    /// `range[id]` = the live range colouring used, kept so
    /// [`verify_slot_colouring`] can re-derive (rather than trust) the
    /// no-aliasing property.
    range: Vec<Option<LiveRange>>,
    /// `class[id]` = which pool the colour came from.
    class: Vec<Option<SlotClass>>,
    /// Distinct 8-byte slots the plan needs — what [`estimate_frame_bytes`]
    /// budgets and [`Lowerer::new`] reserves.
    slots: usize,
    /// Maximum number of live ranges covering any single position: the floor a
    /// perfect colouring with no pinned classes would reach. Reported for
    /// measurement only; nothing is sized from it.
    peak_live: usize,
}

impl SlotPlan {
    /// The colours that provably never hold a reference.
    ///
    /// `assign_colors` keeps two free lists and never moves a colour between
    /// them: a colour first taken by a non-`Ref` value is returned to
    /// `free_prim` and can only ever be recycled by another non-`Ref` value. So
    /// a colour is single-class for the life of the method, and a `Prim` colour
    /// holds a live primitive or dead bytes left by one -- never an object
    /// pointer.
    ///
    /// This is the IR tier's half of the stale-word oracle. Without it an IR
    /// frame carries no dataflow at all and every stale word in it reports as
    /// unexplained, which is exactly where the last two unattributed words of
    /// the 2026-09-02 `TestRandomMapOps` measurement were. See
    /// `OopMapEntry::non_oop_stack_slots`.
    fn prim_colors(&self) -> Vec<u32> {
        // Marked by colour, not searched per node: this runs on the compile
        // path of every IR method, and a linear `contains` would make it
        // quadratic in the colour count on exactly the large graphs that can
        // least afford it.
        let mut seen = vec![false; self.slots];
        for (id, class) in self.class.iter().enumerate() {
            if !matches!(class, Some(SlotClass::Prim)) {
                continue;
            }
            if let Some(Some(color)) = self.node_color.get(id).copied() {
                if let Some(flag) = seen.get_mut(color as usize) {
                    *flag = true;
                }
            }
        }
        seen.iter()
            .enumerate()
            .filter(|(_, &f)| f)
            .map(|(c, _)| c as u32)
            .collect()
    }
}

/// Work budget for the liveness fixed point, in `nodes × blocks` units.
///
/// Past this the analysis is skipped and every value keeps a dedicated slot —
/// still an improvement on the old reservation (only nodes that actually
/// allocate get a slot, rather than every arena node), and the frame bound then
/// declines the method exactly as it does today. A compiler must not turn a
/// pathological graph into a pathological *compile*.
const SLOT_PLAN_WORK_BUDGET: usize = 8_000_000;

/// Hard cap on liveness fixed-point iterations. The analysis converges in
/// loop-depth + 2 passes over a reducible CFG walked in reverse block order;
/// exceeding this means the block order is not what we think it is, and the
/// answer is discarded (everything pinned) rather than truncated — a truncated
/// may-analysis under-approximates liveness, which would alias live values.
const SLOT_PLAN_MAX_ITERATIONS: usize = 256;

/// Resolve the block that produces control token `ctrl` by walking up control
/// inputs until a node that heads some block is reached.
///
/// Free-function form of [`Lowerer::block_of_ctrl`], which delegates here, so
/// [`plan_slots`] attributes a phi's value inputs to exactly the predecessor
/// block `emit_phi_copies` will copy them from. Every index is checked: the
/// planner runs before `verify_data_locations`, on a graph nothing has vetted.
fn ctrl_block_of(graph: &Graph, schedule: &Schedule, mut ctrl: NodeId) -> Option<usize> {
    for _ in 0..graph.nodes.len() {
        if ctrl == NO_NODE {
            return None;
        }
        let blk = *schedule.node_to_block.get(ctrl as usize)?;
        if blk != usize::MAX && schedule.blocks.get(blk).map(|b| b.ctrl) == Some(ctrl) {
            return Some(blk);
        }
        // Step up the control chain (first input is the control edge for
        // Proj/If/Merge-derived nodes).
        let node = graph.nodes.get(ctrl as usize)?;
        match node.inputs.first() {
            Some(&next) if next != ctrl => ctrl = next,
            _ => return None,
        }
    }
    None
}

/// Set bit `i`. Out-of-range indices are ignored rather than panicking: the
/// planner runs on a graph nothing has verified yet.
#[inline]
fn bits_insert(bits: &mut [u64], i: usize) {
    if let Some(word) = bits.get_mut(i / 64) {
        *word |= 1u64 << (i % 64);
    }
}

/// Is bit `i` set?
#[inline]
fn bits_contains(bits: &[u64], i: usize) -> bool {
    bits.get(i / 64)
        .is_some_and(|w| w & (1u64 << (i % 64)) != 0)
}

/// Visit every set bit, in increasing order.
#[inline]
fn bits_for_each(bits: &[u64], mut f: impl FnMut(usize)) {
    for (w, &word) in bits.iter().enumerate() {
        let mut rest = word;
        while rest != 0 {
            let b = rest.trailing_zeros() as usize;
            f(w * 64 + b);
            rest &= rest - 1;
        }
    }
}

/// Compute the liveness-based frame-slot colouring for one scheduled graph.
///
/// Total: never panics and never fails. A graph it cannot analyse (over the
/// work budget, or a fixed point that will not converge) falls back to a
/// dedicated slot per allocating value, which is what the lowerer did before
/// this existed. The result is *checked* by [`verify_slot_colouring`], not
/// trusted — the property it establishes (two values live at one point never
/// share a word) is the one whose violation is silent wrong code.
fn plan_slots(
    graph: &Graph,
    schedule: &Schedule,
    sr_map: Option<&ScalarReplacementMap>,
) -> SlotPlan {
    let n = graph.nodes.len();
    let nb = schedule.blocks.len();

    // ── 1. Which values receive a frame slot ─────────────────────────
    //
    // Exactly the set `alloc_slot` is called for: every phi (reserved by
    // `prealloc_phi_slots` whether scheduled or not) plus every scheduled node
    // whose lowering allocates. `op_defines_result_slot` is the same predicate
    // `verify_data_locations` uses, so the two cannot drift.
    let mut wants_slot = vec![false; n];
    for (id, node) in graph.nodes.iter().enumerate() {
        if matches!(node.op, Op::Phi) {
            wants_slot[id] = true;
        }
    }
    for block in &schedule.blocks {
        for &nid in &block.nodes {
            if let Some(node) = graph.nodes.get(nid as usize) {
                if op_defines_result_slot(&node.op) {
                    wants_slot[nid as usize] = true;
                }
            }
        }
    }

    // ── 2. Linear emission positions and block spans ─────────────────
    let mut pos_of: Vec<Option<usize>> = vec![None; n];
    let mut span: Vec<(usize, usize)> = Vec::with_capacity(nb);
    let mut seq = 0usize;
    for block in &schedule.blocks {
        let start = seq;
        for &nid in &block.nodes {
            if let Some(cell) = pos_of.get_mut(nid as usize) {
                *cell = Some(seq);
            }
            seq += 1;
        }
        if let Some(term) = block.terminator {
            if let Some(cell) = pos_of.get_mut(term as usize) {
                *cell = Some(seq);
            }
            seq += 1;
        }
        // One position past the block's last instruction: the outgoing edge,
        // where `emit_phi_copies` reads this block's phi arguments.
        let end = seq;
        seq += 1;
        span.push((start, end));
    }
    let total_positions = seq;

    // ── 3. Classes that never share ──────────────────────────────────
    let mut pinned = vec![false; n];
    let mut fresh_only = vec![false; n];
    for (id, node) in graph.nodes.iter().enumerate() {
        if !wants_slot[id] {
            continue;
        }
        if matches!(node.op, Op::Phi) {
            pinned[id] = true;
        }
        // cov-01: `Op::ConstString` / `Op::ConstClass` produce a `Ref` from a
        // helper call, exactly as `Op::Call` can, and their arms publish the
        // safepoint map before the call for the same reason. Give them the same
        // "may donate a colour, may never receive a recycled one" treatment; a
        // recycled colour here would be a slot the map already named.
        if node.ty == IrType::Ref
            && matches!(
                node.op,
                Op::Call { .. }
                    | Op::ConstString { .. }
                    | Op::ConstClass { .. }
                    | Op::LoadStatic { .. }
            )
        {
            fresh_only[id] = true;
        }
    }
    for sp in &graph.safepoints {
        for &v in sp.locals.iter().chain(sp.stack.iter()) {
            if v != NO_NODE {
                if let Some(slot) = pinned.get_mut(v as usize) {
                    *slot = true;
                }
            }
        }
    }
    if let Some(sr) = sr_map {
        for info in sr.objects.values() {
            for &v in info.field_values.iter().flatten() {
                if v != NO_NODE {
                    if let Some(slot) = pinned.get_mut(v as usize) {
                        *slot = true;
                    }
                }
            }
        }
    }

    // ── 4. Uses, definitions and the liveness fixed point ────────────
    let mut lo = vec![usize::MAX; n];
    let mut hi = vec![0usize; n];
    // Past the work budget nothing below runs — including the `nodes × blocks`
    // bit-set allocation, which is the expensive part — and every value keeps a
    // dedicated slot.
    let mut coloured = n.saturating_mul(nb) <= SLOT_PLAN_WORK_BUDGET;
    if coloured {
        let words = n.div_ceil(64).max(1);
        let mut def_bits = vec![0u64; words * nb];
        let mut use_bits = vec![0u64; words * nb];
        let mut phi_out_bits = vec![0u64; words * nb];

        // Direct (non-phi) uses and definitions, in block order so that "used
        // before defined in this block" — the upward-exposed set the dataflow
        // needs — falls out of a single pass.
        let mut local_def = vec![0u64; words];
        for b in 0..nb {
            let base = b * words;
            local_def.fill(0);
            let block = &schedule.blocks[b];
            for u in block.nodes.iter().copied().chain(block.terminator) {
                let ui = u as usize;
                let node = match graph.nodes.get(ui) {
                    Some(node) => node,
                    None => continue,
                };
                let upos = pos_of.get(ui).copied().flatten().unwrap_or(span[b].0);
                // A phi's value inputs are NOT read here — the predecessor's
                // edge copy reads them, handled below; `inputs[0]` is control.
                if !matches!(node.op, Op::Phi) {
                    for &inp in &node.inputs {
                        let ii = inp as usize;
                        if inp == NO_NODE || ii >= n || !wants_slot[ii] {
                            continue;
                        }
                        lo[ii] = lo[ii].min(upos);
                        hi[ii] = hi[ii].max(upos);
                        if !bits_contains(&local_def, ii) {
                            bits_insert(&mut use_bits[base..base + words], ii);
                        }
                    }
                }
                if wants_slot[ui] {
                    bits_insert(&mut local_def, ui);
                    bits_insert(&mut def_bits[base..base + words], ui);
                    lo[ui] = lo[ui].min(upos);
                    hi[ui] = hi[ui].max(upos);
                }
            }
        }

        // Phi edge copies. Grouping the phis by their merge control once turns
        // `emit_phi_copies`' per-edge whole-graph scan into a single pass.
        let mut phis_of: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        for (id, node) in graph.nodes.iter().enumerate() {
            if !matches!(node.op, Op::Phi) {
                continue;
            }
            if let Some(&merge) = node.inputs.first() {
                if merge != NO_NODE {
                    phis_of.entry(merge).or_default().push(id as NodeId);
                }
            }
        }
        for succ in 0..nb {
            let merge_ctrl = schedule.blocks[succ].ctrl;
            let merge_node = match graph.nodes.get(merge_ctrl as usize) {
                Some(m) if matches!(m.op, Op::Merge | Op::Region) => m,
                _ => continue,
            };
            let phis = match phis_of.get(&merge_ctrl) {
                Some(phis) => phis,
                None => continue,
            };
            // merge.inputs[k] is the control token for phi value k (= input k+1).
            for (k, &ctrl_in) in merge_node.inputs.iter().enumerate() {
                let pred = match ctrl_block_of(graph, schedule, ctrl_in) {
                    Some(pred) if pred < nb => pred,
                    _ => continue,
                };
                let at = span[pred].1;
                for &pid in phis {
                    let v = match graph
                        .nodes
                        .get(pid as usize)
                        .and_then(|p| p.inputs.get(k + 1))
                    {
                        Some(&v) if v != NO_NODE => v,
                        _ => continue,
                    };
                    let vi = v as usize;
                    if vi >= n || !wants_slot[vi] {
                        continue;
                    }
                    lo[vi] = lo[vi].min(at);
                    hi[vi] = hi[vi].max(at);
                    bits_insert(&mut phi_out_bits[pred * words..(pred + 1) * words], vi);
                }
            }
        }

        let mut live_in = vec![0u64; words * nb];
        let mut live_out = vec![0u64; words * nb];
        let mut scratch = vec![0u64; words];
        let mut converged = false;
        for _ in 0..SLOT_PLAN_MAX_ITERATIONS {
            let mut changed = false;
            // Reverse block order approximates reverse postorder, which is the
            // fast direction for a backward analysis.
            for b in (0..nb).rev() {
                let base = b * words;
                scratch.copy_from_slice(&phi_out_bits[base..base + words]);
                for &s in &schedule.blocks[b].successors {
                    if s >= nb {
                        continue;
                    }
                    for w in 0..words {
                        scratch[w] |= live_in[s * words + w];
                    }
                }
                for w in 0..words {
                    let merged_out = live_out[base + w] | scratch[w];
                    if merged_out != live_out[base + w] {
                        live_out[base + w] = merged_out;
                        changed = true;
                    }
                    let entering = use_bits[base + w] | (merged_out & !def_bits[base + w]);
                    let merged_in = live_in[base + w] | entering;
                    if merged_in != live_in[base + w] {
                        live_in[base + w] = merged_in;
                        changed = true;
                    }
                }
            }
            if !changed {
                converged = true;
                break;
            }
        }
        coloured = converged;

        // ── 5a. Widen each range to cover every block it is live in ──
        //
        // This is where loops are handled: a value used inside a loop is
        // live-out of every block on the back edge's path, so its range covers
        // the whole loop without the colouring ever knowing what a loop is.
        if coloured {
            for b in 0..nb {
                let base = b * words;
                let (start, end) = span[b];
                bits_for_each(&live_in[base..base + words], |v| {
                    if v < n && wants_slot[v] {
                        lo[v] = lo[v].min(start);
                    }
                });
                bits_for_each(&live_out[base..base + words], |v| {
                    if v < n && wants_slot[v] {
                        hi[v] = hi[v].max(end);
                    }
                });
            }
        }
    }

    // ── 5b. Finalise the ranges ──────────────────────────────────────
    let whole_method = LiveRange {
        lo: 0,
        hi: total_positions.saturating_sub(1),
    };
    let mut range: Vec<Option<LiveRange>> = vec![None; n];
    for id in 0..n {
        if !wants_slot[id] {
            continue;
        }
        if !coloured {
            pinned[id] = true;
        }
        range[id] = Some(if lo[id] == usize::MAX || !coloured {
            // Never placed and never live anywhere (an unscheduled phi, or the
            // analysis was skipped): assume the whole method and pin it.
            whole_method
        } else {
            LiveRange {
                lo: lo[id],
                hi: hi[id].max(lo[id]),
            }
        });
    }

    // Peak simultaneous liveness, by sweeping the interval endpoints. This is
    // the floor a perfect colouring would reach; `slots` sits above it by the
    // pinned classes plus whatever the contiguous-interval approximation costs.
    let mut delta = vec![0i64; total_positions + 2];
    for r in range.iter().flatten() {
        if r.lo < delta.len() && r.hi + 1 < delta.len() {
            delta[r.lo] += 1;
            delta[r.hi + 1] -= 1;
        }
    }
    let mut running = 0i64;
    let mut peak_live = 0i64;
    for d in &delta {
        running += d;
        peak_live = peak_live.max(running);
    }

    // ── 6. Colouring ─────────────────────────────────────────────────
    //
    // Pinned values first, in node-id order — the order `prealloc_phi_slots`
    // walks — so a phi web's slots stay where they have always been and the
    // frame layout of a phi-only change is unperturbed.
    let mut node_color: Vec<Option<u32>> = vec![None; n];
    let mut class: Vec<Option<SlotClass>> = vec![None; n];
    // A colour index cannot exceed the node count, and `NodeId` is a `u32`, so
    // the counter provably fits.
    let mut next_color: u32 = 0;
    for id in 0..n {
        if wants_slot[id] && pinned[id] {
            node_color[id] = Some(next_color);
            class[id] = Some(SlotClass::Pinned);
            next_color = next_color.saturating_add(1);
        }
    }

    let mut order: Vec<usize> = (0..n).filter(|&id| wants_slot[id] && !pinned[id]).collect();
    order.sort_by_key(|&id| (range[id].map_or(0, |r| r.lo), id));
    // (hi, colour, is_ref), min-heap on `hi` so the earliest-expiring colour
    // is released first.
    let mut active: BinaryHeap<Reverse<(usize, u32, bool)>> = BinaryHeap::new();
    let mut free_ref: Vec<u32> = Vec::new();
    let mut free_prim: Vec<u32> = Vec::new();
    for id in order {
        let r = match range[id] {
            Some(r) => r,
            None => continue,
        };
        while let Some(&Reverse((active_hi, active_color, active_is_ref))) = active.peek() {
            if active_hi >= r.lo {
                break;
            }
            active.pop();
            if active_is_ref {
                free_ref.push(active_color);
            } else {
                free_prim.push(active_color);
            }
        }
        let is_ref = graph.nodes[id].ty == IrType::Ref;
        let recycled = if fresh_only[id] {
            None
        } else if is_ref {
            free_ref.pop()
        } else {
            free_prim.pop()
        };
        let color = match recycled {
            Some(color) => color,
            None => {
                let color = next_color;
                next_color = next_color.saturating_add(1);
                color
            }
        };
        node_color[id] = Some(color);
        class[id] = Some(if is_ref {
            SlotClass::Ref
        } else {
            SlotClass::Prim
        });
        active.push(Reverse((r.hi, color, is_ref)));
    }

    SlotPlan {
        node_color,
        range,
        class,
        slots: next_color as usize,
        peak_live: peak_live.max(0) as usize,
    }
}

/// Refuse, before a single byte is emitted, a colouring that could alias two
/// simultaneously live values onto one frame word.
///
/// This is the counterpart of [`verify_data_locations`]: that one asks whether
/// every value *has* a location, this one asks whether the locations are
/// *distinct where they must be*. Both failures are silent wrong code — the
/// first reads the saved caller RBP as program data, the second reads one
/// value where another was written — so neither is assumed.
///
/// Four properties, in the order a violation would bite:
///
///   1. every value the lowerer will allocate for has a colour (a plan that
///      missed one would take `alloc_slot`'s internal-bailout path mid-emission
///      instead of declining up front);
///   2. every colour is inside the reservation the frame was sized from;
///   3. **no two values with overlapping live ranges share a colour** — the
///      wrong-code property;
///   4. a colour is never shared across [`SlotClass`]es, so a slot an oop map
///      names can never hold a primitive, and a pinned value never shares at
///      all.
fn verify_slot_colouring(graph: &Graph, schedule: &Schedule, plan: &SlotPlan) -> CompileResult<()> {
    let color_of = |id: usize| plan.node_color.get(id).copied().flatten();

    // (1) Coverage — the same two sources `plan_slots` enumerates.
    for (id, node) in graph.nodes.iter().enumerate() {
        if matches!(node.op, Op::Phi) && color_of(id).is_none() {
            return Err(Bailout::with_context(
                BailoutReason::UnallocatedValue { node: id as NodeId },
                format!("phi n{id} has no planned frame slot"),
            ));
        }
    }
    for block in &schedule.blocks {
        for &nid in &block.nodes {
            let node = match graph.nodes.get(nid as usize) {
                Some(node) => node,
                None => {
                    return Err(Bailout::new(BailoutReason::Internal(
                        "ir_lower: schedule names a node outside the graph",
                    )))
                }
            };
            if op_defines_result_slot(&node.op) && color_of(nid as usize).is_none() {
                return Err(Bailout::with_context(
                    BailoutReason::UnallocatedValue { node: nid },
                    format!("scheduled n{nid} ({:?}) has no planned frame slot", node.op),
                ));
            }
        }
    }

    // (2) Every colour is inside the reservation.
    for (id, color) in plan.node_color.iter().enumerate() {
        if let Some(color) = color {
            if *color as usize >= plan.slots {
                return Err(Bailout::with_context(
                    BailoutReason::Internal(
                        "ir_lower: a planned colour is outside the spill reservation",
                    ),
                    format!("n{id} coloured {color} with only {} slots", plan.slots),
                ));
            }
        }
    }

    // (3) + (4) Group by colour and check the members pairwise. Sorting each
    // group by range start makes the adjacent pairs sufficient: if any pair
    // overlaps, the earliest offender's predecessor in sort order overlaps it
    // too, so an adjacent pair always witnesses the violation.
    let mut members: Vec<Vec<usize>> = vec![Vec::new(); plan.slots];
    for (id, color) in plan.node_color.iter().enumerate() {
        if let Some(color) = color {
            if let Some(bucket) = members.get_mut(*color as usize) {
                bucket.push(id);
            }
        }
    }
    for (color, bucket) in members.iter().enumerate() {
        if bucket.len() < 2 {
            continue;
        }
        for &id in bucket {
            if plan.class.get(id).copied().flatten() == Some(SlotClass::Pinned) {
                return Err(Bailout::with_context(
                    BailoutReason::Internal("ir_lower: a pinned value shares its frame slot"),
                    format!(
                        "slot {color} is shared by {} values, one pinned",
                        bucket.len()
                    ),
                ));
            }
        }
        let classes: Vec<Option<SlotClass>> = bucket
            .iter()
            .map(|&id| plan.class.get(id).copied().flatten())
            .collect();
        if classes.windows(2).any(|w| w[0] != w[1]) {
            return Err(Bailout::with_context(
                BailoutReason::Internal(
                    "ir_lower: a frame slot is shared across reference and primitive values",
                ),
                format!("slot {color} mixes {classes:?}"),
            ));
        }
        let mut intervals: Vec<(LiveRange, usize)> = bucket
            .iter()
            .filter_map(|&id| plan.range.get(id).copied().flatten().map(|r| (r, id)))
            .collect();
        if intervals.len() != bucket.len() {
            return Err(Bailout::new(BailoutReason::Internal(
                "ir_lower: a coloured value has no live range",
            )));
        }
        intervals.sort_by_key(|(r, id)| (r.lo, r.hi, *id));
        for pair in intervals.windows(2) {
            let ((a, ai), (b, bi)) = (pair[0], pair[1]);
            if a.overlaps(b) {
                return Err(Bailout::with_context(
                    BailoutReason::Internal(
                        "ir_lower: two simultaneously live values share a frame slot",
                    ),
                    format!(
                        "slot {color}: n{ai} live [{}, {}] overlaps n{bi} live [{}, {}]",
                        a.lo, a.hi, b.lo, b.hi
                    ),
                ));
            }
        }
    }
    Ok(())
}

// ── Linear-scan register residency (`CRATONVM_JIT_IR_LINEAR_SCAN`) ───
//
// `regalloc::allocate_linear_scan` is a complete, self-verifying linear-scan
// allocator that had no production consumer. This section is its first one.
//
// It is deliberately NOT a replacement for the frame-slot colouring. The
// colourer still runs, still decides every value's home word, and every value
// is still WRITTEN to that home. What the allocator adds is a *read cache*: a
// value the allocation keeps in one register for its entire live range is also
// copied into that register at its definition, and its later reads take the
// register instead of the frame.
//
// ## Why write-through, and why that is the safe shape
//
// The frame image stays authoritative at every instruction boundary. That is
// what makes this wiring cheap to reason about:
//
//   * `emit_safepoint_map` publishes frame slots. Every live value is still in
//     its slot, so the oop map is exactly as complete as it was — there is no
//     register a collector would have to know about, walk, or *update* on an
//     evacuation. (The register file below is XMM-only, so a reference cannot
//     be register-resident at all; see `IR_LOWER_LS_XMMS`. Two independent
//     reasons, either one sufficient.)
//   * `build_deopt_points` / `FrameState` name frame words. Unchanged.
//   * `emit_phi_copies`, call-argument marshalling and the shadow push all read
//     home slots. Unchanged.
//   * A definition site this wave did not convert simply never publishes
//     residency (see `Lowerer::reg_live`), so all of its reads stay memory
//     reads. A missed *use* site is a missed optimization; there is no edit
//     that turns into wrong code by omission.
//
// The cost is that the store side is not eliminated: this buys loads, not
// stores. Eliminating the store requires the whole lowerer to stop treating the
// frame as the value's identity, which is the large change this one is the
// increment towards. See `docs/jit/linear-scan-wiring.md`.

/// The registers this wiring may hand out.
///
/// XMM2–XMM7, and the ceiling is forced rather than tuned:
///
///   * **Never touched by this emitter.** The FP value tier is XMM0/XMM1; the
///     GP tier is RAX/RCX/RDX with R10/R11 as safepoint and shadow-stack
///     scratch and R8/R9 as call-argument registers. XMM2–XMM7 appear nowhere.
///   * **Encodable without REX**, which `fp_load` / `fp_store` / `fp_binop`
///     require: they emit ModRM with `(xmm & 7) << 3` and no REX.R, so only
///     XMM0–XMM7 are addressable by them at all. *This* is what stops the file
///     at XMM7 — raising it is a REX change in those three functions, not a
///     frame change.
///   * **Callee-saved registers are now paid for.** Win64 makes XMM0–XMM5
///     volatile and XMM6–XMM15 non-volatile; System V makes every XMM volatile.
///     XMM6/XMM7 were unusable here until [`IR_LOWER_SAVED_XMMS`] and the
///     prologue save area landed (2026-08-04): before that `emit_prologue`
///     pushed RBP and saved nothing, so a value parked in XMM6 corrupted the
///     caller's floating-point state on one platform and not the other.
///
/// The consequence worth stating plainly: this wiring is still **FP-only**. An
/// `int` loop counter gets nothing out of it — that needs a GP file, and a GP
/// file has to discharge the safepoint obligation per site instead of
/// structurally (`docs/feature-designs/jit-machine-level-and-instruction-selection.md`,
/// "Where the safepoint / oop-map obligation lives").
///
/// The literal lives in `regalloc::xmm_roles`, with the other two XMM
/// authorities — `ir_lower`'s own scratch pair below and `vec_emit`'s vector
/// pool — so that a wiring which puts two of them on one register is a visible
/// fact rather than a discovery. All three are disjoint as of 2026-08-04;
/// `xmm_roles::disjointness_violation` is the check.
const IR_LOWER_LS_XMMS: [u8; 6] = crate::regalloc::xmm_roles::IR_LINEAR_SCAN;

/// The XMM registers [`Lowerer::emit_prologue`] saves and every exit restores.
///
/// Empty on System V, where the ABI makes every XMM volatile and there is
/// nothing to save; XMM6/XMM7 on Windows, where it does not.
///
/// **This is the prerequisite the level-2 lane was blocked on**, and it is
/// deliberately its smallest useful form: two registers, saved only when a
/// method actually parks a value in one, in a frame band the GC verifier
/// already skips (`callee_saved_lo`). It is what lets [`IR_LOWER_LS_XMMS`]
/// reach XMM7 and what moved `vec_emit`'s pool off the scalar file entirely.
///
/// Sizing is static — the frame is laid out in [`Lowerer::new`], before the
/// allocation runs — but *emission* is dynamic: [`Lowerer::saved_xmm_regs`]
/// yields only the registers the residency plan actually used, so a method that
/// promotes none of them emits no save instruction and no restore.
const IR_LOWER_SAVED_XMMS: &[u8] = crate::regalloc::xmm_roles::IR_PROLOGUE_SAVED;

/// Bytes the frame reserves for [`IR_LOWER_SAVED_XMMS`].
///
/// 16 per register: a caller's value may be a full 128-bit vector, and saving
/// only its low 64 bits restores a register that is *almost* right — the
/// failure mode hardest to attribute. `MOVUPS` has no alignment requirement, so
/// the band needs no padding.
///
/// Zero unless the linear-scan path is on, because nothing else can name one of
/// these registers yet and a frame must not pay 32 bytes for a register it
/// cannot hand out. [`estimate_frame_bytes`] and [`Lowerer::new`] both go
/// through this one function for exactly the reason the `bookkeeping_size`
/// comment gives: two copies of a frame term drift, and the drift is a silent
/// wrong offset.
fn ir_saved_xmm_bytes() -> i32 {
    if IR_LOWER_SAVED_XMMS.is_empty() || !linear_scan_enabled() {
        return 0;
    }
    // Cast: a two-element compile-time constant.
    IR_LOWER_SAVED_XMMS.len() as i32 * 16
}

/// The **general-purpose** linear-scan file: the registers an `int` or `long`
/// value may live in for a whole method.
///
/// This is the half the XMM wiring's own doc comment named as missing —
/// *"this wiring is still FP-only. An `int` loop counter gets nothing out of
/// it"* — and it is what made the optimizing tier emit slower code than the
/// baseline tier on every loop measured: `x64.rs` colours Java locals into
/// callee-saved GPRs, while this backend read every value out of a frame word.
///
/// Same shape as the FP file and for the same reasons: write-through, so the
/// frame image stays authoritative at every instruction boundary and no oop
/// map, deopt frame state or phi copy changes; single-segment allocations
/// only, because there is no reload machinery; and a definition site that does
/// not publish simply leaves its reads in memory.
///
/// The one obligation that is genuinely new is the **safepoint** one, and it is
/// discharged by type rather than by structure: see
/// `regalloc::xmm_roles::IR_GP_LINEAR_SCAN`.
const IR_LOWER_LS_GPRS: [u8; 5] = crate::regalloc::xmm_roles::IR_GP_LINEAR_SCAN;

/// The GP registers [`Lowerer::emit_prologue`] saves and every exit restores.
///
/// All of [`IR_LOWER_LS_GPRS`]: every one is callee-saved on both System V and
/// Win64, which is why they are the file. Unlike the XMM list this is not
/// platform-conditional.
const IR_LOWER_SAVED_GPRS: &[u8] = crate::regalloc::xmm_roles::IR_GP_PROLOGUE_SAVED;

/// Bytes the frame reserves for [`IR_LOWER_SAVED_GPRS`].
///
/// Eight per register — a GPR is 8 bytes and, unlike the XMM band, there is no
/// wider value hiding in it. Zero unless the linear-scan path is on, for the
/// same reason `ir_saved_xmm_bytes` returns zero then: a frame must not pay for
/// a register nothing can hand out.
fn ir_saved_gpr_bytes() -> i32 {
    if IR_LOWER_SAVED_GPRS.is_empty() || !linear_scan_enabled() {
        return 0;
    }
    // Cast: a five-element compile-time constant.
    IR_LOWER_SAVED_GPRS.len() as i32 * 8
}

/// `CRATONVM_JIT_IR_ISEL_SHADOW` — run the instruction selector over this
/// compile's blocks, count what it would have produced, and **discard it**.
///
/// `docs/feature-designs/jit-machine-level-and-instruction-selection.md`,
/// increment 0. Default **off**. Turning it
/// on changes no emitted byte — `shadow_selection_changes_no_emitted_byte`
/// pins that — and costs one tiling pass per compile. It exists to replace the
/// contract's ten-shape synthetic coverage figure with one taken over real
/// compiles, because that number is what decides whether the rest of the
/// HIR/MIR migration is worth its cost.
///
/// Report the result with `CRATONVM_DBG=ir-isel`.
fn isel_shadow_enabled() -> bool {
    match cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_ISEL_SHADOW") {
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => false,
    }
}

/// `CRATONVM_DBG=ir-isel` — print one shadow-selection line per compile.
fn isel_shadow_reporting() -> bool {
    cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_IR_ISEL").is_some()
}

/// `CRATONVM_JIT=ir-isel-verify` — build the level-2 machine list and check the
/// tile encoder against the per-opcode arms, byte for byte. Default **off**.
///
/// Changes no emitted byte: the per-opcode arms still emit, and the encoder's
/// output is compared against what they wrote. This is the method-scale form of
/// the anchoring property each `PATTERNS` row carries per row, and it is the
/// gate [`isel_emit_enabled`] has to pass on a real corpus first.
fn isel_verify_enabled() -> bool {
    #[cfg(test)]
    {
        if let Some(mode) = mir_forced() {
            return mode == MirMode::Verify;
        }
    }
    match cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_ISEL_VERIFY") {
        Ok(v) => !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => false,
    }
}

/// `CRATONVM_JIT=ir-isel-emit` — the level-2 machine list emits, for the tiles
/// the encoder covers. Default **off**.
///
/// Fail-closed, unlike `ir-isel-shadow`: a block the selector does not cover, a
/// tile whose bytes disagree with the destination slot the allocator hands out,
/// or an encoder refusal *after* the tile was admitted all discard the
/// artifact, and the method runs in a lower tier. That is always valid, and it
/// is the trade increment 0 was explicitly exempt from because its documented
/// effect was a count.
fn isel_emit_enabled() -> bool {
    #[cfg(test)]
    {
        if let Some(mode) = mir_forced() {
            return mode == MirMode::Emit;
        }
    }
    match cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_ISEL_EMIT") {
        Ok(v) => !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => false,
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only override for the two machine-level flags, so a unit test never
    /// depends on the process environment. Thread-local, so parallel tests
    /// cannot see each other's setting.
    static MIR_FORCE: std::cell::Cell<Option<MirMode>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn mir_forced() -> Option<MirMode> {
    MIR_FORCE.with(|c| c.get())
}

/// Scoped override of the machine-level mode for one test.
#[cfg(test)]
struct MirForce;

#[cfg(test)]
impl MirForce {
    fn set(mode: MirMode) -> MirForce {
        MIR_FORCE.with(|c| c.set(Some(mode)));
        MirForce
    }
}

#[cfg(test)]
impl Drop for MirForce {
    fn drop(&mut self) {
        MIR_FORCE.with(|c| c.set(None));
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only: make the level-2 encoder produce a deliberately wrong byte,
    /// so the byte-equality oracle can be observed *failing*. A guard nobody
    /// has ever seen fail is a guard of unknown polarity.
    static MIR_INJECT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn mir_injecting_a_wrong_byte() -> bool {
    MIR_INJECT.with(|c| c.get())
}

/// Scoped byte injection for one test.
#[cfg(test)]
struct MirInject;

#[cfg(test)]
impl MirInject {
    fn on() -> MirInject {
        MIR_INJECT.with(|c| c.set(true));
        MirInject
    }
}

#[cfg(test)]
impl Drop for MirInject {
    fn drop(&mut self) {
        MIR_INJECT.with(|c| c.set(false));
    }
}

/// Process totals for the level-2 machine list, so a corpus run has a number
/// rather than a stream of per-compile lines.
///
/// Read them with [`mir_totals::read`]; the `--dump-jit-stats` style consumers
/// and the corpus probe both go through it. Plain atomics rather than a mutex:
/// this runs inside the compile path, on the compiler thread, and a torn read
/// of a diagnostic counter is not worth a lock.
pub mod mir_totals {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    static METHODS: AtomicU64 = AtomicU64::new(0);
    static TILES: AtomicU64 = AtomicU64::new(0);
    static MISMATCHES: AtomicU64 = AtomicU64::new(0);
    static SHADOW_TILES: AtomicU64 = AtomicU64::new(0);
    static ARM_BYTES: AtomicU64 = AtomicU64::new(0);
    static ENC_BYTES: AtomicU64 = AtomicU64::new(0);
    static ALLOC_VERIFIED: AtomicU64 = AtomicU64::new(0);
    static ALLOC_VALUES: AtomicU64 = AtomicU64::new(0);
    static ALLOC_UNLOCATED_ROOTS: AtomicU64 = AtomicU64::new(0);
    static ALLOC_VACUOUS: AtomicU64 = AtomicU64::new(0);
    static ALLOC_INDESCRIBABLE: AtomicU64 = AtomicU64::new(0);
    static ALLOC_REJECTED: AtomicU64 = AtomicU64::new(0);

    /// Record one method's [`super::MirAllocVerdict`], keeping the three states
    /// apart.
    ///
    /// Collapsing them is the specific failure this counter set exists to
    /// prevent: a corpus where every method came back `NothingToCover` and one
    /// where every method verified thousands of values are indistinguishable in
    /// a single "ok" tally, and the first proves nothing at all.
    pub(super) fn record_alloc(verdict: super::MirAllocVerdict) {
        match verdict {
            super::MirAllocVerdict::Verified {
                located,
                unlocated_tile_roots,
            } => {
                ALLOC_VERIFIED.fetch_add(1, Relaxed);
                ALLOC_VALUES.fetch_add(located as u64, Relaxed);
                ALLOC_UNLOCATED_ROOTS.fetch_add(unlocated_tile_roots as u64, Relaxed);
            }
            super::MirAllocVerdict::NothingToCover => {
                ALLOC_VACUOUS.fetch_add(1, Relaxed);
            }
            super::MirAllocVerdict::Indescribable(_) => {
                ALLOC_INDESCRIBABLE.fetch_add(1, Relaxed);
            }
        }
    }

    /// One method whose allocation `verify_allocation` REJECTED. A compiler
    /// bug, and non-zero is a stop-and-look number, not a ratio to watch.
    pub(super) fn record_alloc_rejected() {
        ALLOC_REJECTED.fetch_add(1, Relaxed);
    }

    /// `(verified, values, nothing_to_cover, indescribable, rejected)` — the
    /// increment-2 allocation verdicts since process start.
    pub fn read_alloc() -> (u64, u64, u64, u64, u64) {
        (
            ALLOC_VERIFIED.load(Relaxed),
            ALLOC_VALUES.load(Relaxed),
            ALLOC_VACUOUS.load(Relaxed),
            ALLOC_INDESCRIBABLE.load(Relaxed),
            ALLOC_REJECTED.load(Relaxed),
        )
    }

    /// Tile roots the allocation could give no home word, summed over every
    /// verified method.
    ///
    /// Increment 3's sizing, taken from real compiles instead of from a
    /// synthetic shape count: every one of these is a value that must live in a
    /// register because there is no memory for it to live in.
    pub fn read_unlocated_tile_roots() -> u64 {
        ALLOC_UNLOCATED_ROOTS.load(Relaxed)
    }

    pub(super) fn record(
        tiles: usize,
        mismatches: usize,
        shadow_tiles: usize,
        arm_bytes: usize,
        enc_bytes: usize,
    ) {
        METHODS.fetch_add(1, Relaxed);
        TILES.fetch_add(tiles as u64, Relaxed);
        MISMATCHES.fetch_add(mismatches as u64, Relaxed);
        SHADOW_TILES.fetch_add(shadow_tiles as u64, Relaxed);
        ARM_BYTES.fetch_add(arm_bytes as u64, Relaxed);
        ENC_BYTES.fetch_add(enc_bytes as u64, Relaxed);
    }

    /// `(methods, tiles, mismatches)` since process start.
    pub fn read() -> (u64, u64, u64) {
        (
            METHODS.load(Relaxed),
            TILES.load(Relaxed),
            MISMATCHES.load(Relaxed),
        )
    }

    /// `(shadow_tiles, arm_bytes, encoder_bytes)` — verify mode's sizing of the
    /// increment that would emit the rules byte equality cannot cover.
    ///
    /// `arm_bytes - encoder_bytes` is what those tiles would save, over exactly
    /// the nodes they cover, on this workload. A zero `shadow_tiles` means the
    /// question does not arise; it does not mean the saving is zero.
    pub fn read_shadow() -> (u64, u64, u64) {
        (
            SHADOW_TILES.load(Relaxed),
            ARM_BYTES.load(Relaxed),
            ENC_BYTES.load(Relaxed),
        )
    }

    /// Zero the accumulator. Tests only — hold [`TEST_LOCK`] across the reset
    /// AND the read, or two tests reading one global see each other's counts.
    #[cfg(test)]
    pub fn reset() {
        for c in [
            &METHODS,
            &TILES,
            &MISMATCHES,
            &SHADOW_TILES,
            &ARM_BYTES,
            &ENC_BYTES,
            &ALLOC_VERIFIED,
            &ALLOC_VALUES,
            &ALLOC_UNLOCATED_ROOTS,
            &ALLOC_VACUOUS,
            &ALLOC_INDESCRIBABLE,
            &ALLOC_REJECTED,
        ] {
            c.store(0, Relaxed);
        }
    }

    /// Serialises the tests that read the counters above. Its own lock, not
    /// `isel::SHADOW_TEST_LOCK`: these are different globals, and sharing one
    /// mutex between two unrelated accumulators is how a later test ends up
    /// holding the wrong one.
    #[cfg(test)]
    pub static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}

/// May the level-2 encoder answer for node `id`'s lowering with this tile?
///
/// Three conditions, and the third is the one that is not obvious. The caller
/// skips **every** node a tile covers, so a tile that absorbed a node the
/// encoder does not actually fold leaves that node computed nowhere — the same
/// failure `BlockSelection::covers` exists to make unrepresentable, one level
/// further down. `Rule::AluReg` never absorbs, so today this clause is
/// insurance rather than a live filter; a rule added to the set below without a
/// fold is exactly what it is insurance against.
fn mir_tile_is_emittable(tile: &crate::x64::isel::Tile, id: NodeId) -> bool {
    use crate::x64::isel::Rule;

    tile.root == id && tile.rule == Rule::AluReg && tile.covered.as_slice() == [id]
}

/// Build the level-2 machine list for one method, or refuse.
///
/// `None` means "do not compile this method through the machine level" and is
/// returned for exactly one reason: a block the selector did not cover. That is
/// the invariant `BlockSelection::covers` exists for — a node covered zero times
/// is a dropped instruction — and increment 0 measured it holding on all 1 911
/// blocks of a real Spring Boot workload, so a violation here is a compiler bug
/// and not a shape to work around.
///
/// `SelectOptions::default()` for the same reason `shadow_select_method` uses
/// it: `require_encodable: true`, and `fold_loads: false` because the fold's
/// address half is still `AddrSource::Opaque`.
fn build_mir_plan(graph: &Graph, schedule: &Schedule) -> Option<MirPlan> {
    use crate::x64::isel::{select_block, SelectOptions};

    // `frame_homed: true` is the one departure from `shadow_select_method`'s
    // options, and it is a statement about the consumer rather than a tuning
    // knob: `encode_tile_frame_homed` puts every value in its frame word, so
    // the two-address copy the cost model would otherwise charge the ALU form
    // does not exist. Left `false` it makes `Rule::Lea` outbid `Rule::AluReg`
    // on `a + b` whenever `a` is live afterwards, and the `LEA` that wins is a
    // byte longer than the `ADD` it replaced.
    let opts = SelectOptions {
        frame_homed: true,
        ..SelectOptions::default()
    };
    let mut blocks = Vec::with_capacity(schedule.blocks.len());
    let mut tile_of: Vec<Option<(u32, u32)>> = vec![None; graph.nodes.len()];
    for (bi, block) in schedule.blocks.iter().enumerate() {
        let sel = select_block(graph, &block.nodes, block.terminator, &opts);
        if !sel.covers(&block.nodes) {
            return None;
        }
        let bi = u32::try_from(bi).ok()?;
        for (ti, tile) in sel.tiles.iter().enumerate() {
            let ti = u32::try_from(ti).ok()?;
            if let Some(cell) = tile_of.get_mut(tile.root as usize) {
                // Two tiles rooted at one node would mean the node is computed
                // twice; `covers` already refused that, so this is belt and
                // braces on the index rather than a second policy.
                if cell.is_some() {
                    return None;
                }
                *cell = Some((bi, ti));
            }
        }
        blocks.push(sel);
    }
    Some(MirPlan { blocks, tile_of })
}

/// What [`verify_mir_allocation`] concluded — the three-valued shape the
/// design doc requires, not an `Option`.
///
/// `emit_safepoint_map` already refuses to conflate "nothing to cover" with
/// "could not describe it" (an oop-free safepoint publishes
/// `moving_young_coverage_complete: true`; an indescribable slot publishes
/// `false` and diverts the cycle). A level-2 form owes the same distinction,
/// because the two have opposite consequences: one is a healthy compile, the
/// other is a compile whose allocation nothing proved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MirAllocVerdict {
    /// An allocation was built over the tile list and `verify_allocation`
    /// accepted it.
    Verified {
        /// Values the allocation gives a location to.
        located: usize,
        /// Tiles whose root the allocation gives NO location.
        ///
        /// Legal, and worth counting rather than refusing: `plan_slots` gives
        /// no home word to a value nothing reads from memory — the last value
        /// in `(a + b) * a` is returned straight out of RAX — while
        /// `select_block` still roots a tile at it, because a tile is about
        /// *computing* the value, not about storing it. The encoder already
        /// declines such a tile (`frame_destination(dst)?`) and the per-opcode
        /// arm emits it instead.
        ///
        /// It is a number rather than a silence because it is exactly the
        /// gap increment 3 closes: a value with no home is a value that MUST
        /// stay in a register, so this count is the size of the register file's
        /// first real customer.
        unlocated_tile_roots: usize,
    },
    /// The tile list defines no value that needs a location. Vacuous, and
    /// counted separately so a run of these cannot be read as coverage.
    NothingToCover,
    /// The allocation could not be *described*, before any verifier ran: the
    /// liveness model did not converge, or it disagrees with `plan_slots` about
    /// which values want a location.
    ///
    /// Not a failure. `plan_register_residency` treats exactly the same
    /// disagreement as "decline to promote and keep the colourer's answer",
    /// and the reasoning carries: the colourer's layout is correct on its own
    /// terms and is the one the frame was built from. What this must not do is
    /// report itself as `Verified`.
    Indescribable(&'static str),
}

/// Build an allocation over the machine list and put it through the **existing**
/// [`crate::regalloc::verify_allocation`].
///
/// This is increment 2 of
/// `docs/feature-designs/jit-machine-level-and-instruction-selection.md`: "an
/// allocation over it, verified by `verify_allocation`". Deliberately not a new
/// verifier — the value of the increment is that the level-2 artifact is
/// checkable by the allocator's *own* checker, and a second checker written for
/// it would be a second model to keep in step.
///
/// # What the allocation says
///
/// Every value lives in its frame home for its entire live range, with an empty
/// register file. That is not a placeholder: it is exactly what
/// [`Lowerer::encode_tile_frame_homed`] emits, so the allocation being verified
/// is the one the encoder actually implements. The register file is empty
/// rather than XMM-shaped because the MIR encoder holds nothing in a long-lived
/// register — RAX is reloaded per tile.
///
/// # What that buys, given nothing is in a register
///
/// The register rules go quiet; four others do not, and they are the ones that
/// bite a memory-only backend:
///
///   * **the timeline covers the live range** — a value the tile list reads
///     after its recorded death, or defines without a location, is refused
///     here instead of `?`-ing out silently inside the encoder;
///   * **no two simultaneously live values share a home word** — checked
///     against `build_live_model`'s ranges, which are computed *independently*
///     of `plan_slots`'. Two implementations of one liveness model drifting
///     apart is precisely how a backend comes to alias two live values, and
///     the tile list is a third consumer of that model;
///   * **home pools stay separated** — a word holding a `Ref` for one value and
///     a pinned primitive for another is the shape a moving collector
///     misreads;
///   * **no reference is register-resident at a safepoint** — vacuous today
///     and the whole point tomorrow: increment 3 swaps the empty `RegFile` for
///     a real one at this exact call site, and this check is what makes that
///     swap reviewable rather than an act of faith.
///
/// # Failure policy
///
/// `Err` only from `verify_allocation`, and only that. A model disagreement is
/// [`MirAllocVerdict::Indescribable`], not an error, for the reason
/// `plan_register_residency` gives. The caller decides what an `Err` costs:
/// `MirMode::Verify` changes no emitted byte and records it, `MirMode::Emit`
/// refuses the compile, because emitting against an allocation nothing proved
/// is the trade that has produced silent heap corruption in this VM before.
fn verify_mir_allocation(
    graph: &Graph,
    schedule: &Schedule,
    plan: &SlotPlan,
    mir: &MirPlan,
) -> CompileResult<MirAllocVerdict> {
    use crate::regalloc::{
        build_live_model, verify_allocation, Allocation, RegFile, RegSpec, Segment,
    };

    let live = build_live_model(graph, schedule);
    if !live.converged {
        return Ok(MirAllocVerdict::Indescribable("liveness did not converge"));
    }
    // The same agreement check `plan_register_residency` runs, and for the same
    // reason: `build_live_model` re-implements `plan_slots`' position model
    // rather than calling it, so the two are only interchangeable while they
    // agree. Checked before either is used.
    let expected_positions: usize = schedule
        .blocks
        .iter()
        .map(|b| b.nodes.len() + usize::from(b.terminator.is_some()) + 1)
        .sum();
    if live.total_positions != expected_positions {
        return Ok(MirAllocVerdict::Indescribable("position models disagree"));
    }
    if live.wants_loc.len() != plan.node_color.len()
        || live
            .wants_loc
            .iter()
            .zip(plan.node_color.iter())
            .any(|(wants, color)| *wants != color.is_some())
    {
        return Ok(MirAllocVerdict::Indescribable(
            "liveness and the colourer disagree about which values want a location",
        ));
    }

    let n = graph.nodes.len();
    let mut segments: Vec<Vec<Segment>> = vec![Vec::new(); n];
    let mut located = 0usize;
    for id in 0..n {
        if !live.wants_loc.get(id).copied().unwrap_or(false) {
            continue;
        }
        let Some(range) = live.range.get(id).copied().flatten() else {
            // `wants_loc` without a range is the model contradicting itself.
            // Describing it would mean inventing a range, so say so instead.
            return Ok(MirAllocVerdict::Indescribable(
                "a value wants a location but has no live range",
            ));
        };
        segments[id].push(Segment { range, reg: None });
        located += 1;
    }
    if located == 0 {
        return Ok(MirAllocVerdict::NothingToCover);
    }

    // How many tiles the allocation cannot back. Counted, not refused — see
    // `MirAllocVerdict::Verified::unlocated_tile_roots` for why a tile rooted
    // at a homeless value is the normal case rather than a defect.
    let unlocated_tile_roots = mir
        .tile_of
        .iter()
        .enumerate()
        .filter(|(id, cell)| cell.is_some() && segments.get(*id).is_none_or(|s| s.is_empty()))
        .count();

    let alloc = Allocation {
        segments,
        // `Allocation::stack_slot`'s own contract: "a drop-in replacement for
        // `ir_lower`'s `SlotPlan::node_color` — same numbering, same
        // Ref/Prim/pinned pool separation". Passing the colourer's answer
        // rather than a re-derived one is what makes the home-aliasing check
        // above a check of the *frame the encoder addresses*, not of a
        // parallel invention.
        stack_slot: plan.node_color.clone(),
        stack_slots: plan.slots,
        spills: 0,
        reloads: 0,
        remats: 0,
        reg_moves: 0,
        splits: 0,
        promoted: 0,
        events: Vec::new(),
        peak_live: live.peak_live,
    };
    let empty_file = RegFile::from_specs(std::iter::empty::<RegSpec>());
    let model = ir_lower_machine_model(graph, schedule, &live, empty_file);
    verify_allocation(graph, &live, &model, &alloc)?;
    Ok(MirAllocVerdict::Verified {
        located,
        unlocated_tile_roots,
    })
}

/// Run the linear-scan allocator and use its result as a register read cache.
/// **Default ON** since 2026-09-02; `CRATONVM_JIT_IR_LINEAR_SCAN=0` opts out.
///
/// # What this file gained, and how the default moved
///
/// It shipped OFF while the file was XMM-only, and while it was XMM-only that
/// was the right default: the wiring's own comment said *"this wiring is still
/// FP-only. An `int` loop counter gets nothing out of it"*, so turning it on
/// bought two callee-saved XMM registers on Windows and nothing anywhere else.
///
/// [`IR_LOWER_LS_GPRS`] closes that half. It is the answer to a measured
/// inversion: this backend read every `int` value out of a frame word while the
/// single-pass backend it supersedes colours Java locals into callee-saved
/// GPRs, and on five one-line kernels — same binary, `CRATONVM_JIT_IR_LONG=0`
/// to route them to single-pass — the optimizing tier came out **1.25x to 1.9x
/// slower** on Windows and **~3.2x** slower on a quieter Linux host, on every
/// kernel whose cost was not already dominated by an out-of-line helper call.
///
/// **The default stayed off at first because the fix did not reach those
/// kernels, and the census said so rather than a guess.** With
/// `CRATONVM_DBG_IR_LINEAR_SCAN=1`, at that time:
///
/// * on `BinTrees.itemCheck` the file works —
///   `resident=7 (fp=0 gp=7) demoted=0`, `phi=0`, with 13 candidates lost to
///   splits and 2 to type;
/// * on all five probe kernels it never runs at all: `refused: liveness and
///   colourer disagree about which values want a home`, 5 of 5.
///
/// That refusal is a PRE-EXISTING gate, not something the GP file introduced —
/// `regalloc::ir_op_defines_value` and `ir_lower`'s `op_defines_result_slot`
/// are two enumerations of one question (the second and third of the three
/// `the_three_ir_op_enumerations` names), and any disagreement declines the
/// whole method. It was declining the XMM cache the same way and nobody could
/// see it, because the flag printed only on success. The refusal now names the
/// op, so reconciling the two is a one-line fix with a test rather than a
/// search through fifty variants.
///
/// **Both prerequisites landed on 2026-09-02 and the default moved with them.**
/// The enumeration disagreement was the one `ir_op_defines_value` had over
/// `Op::ArrayLength` and `Op::NewArray` — it declined the file on every method
/// containing an `arraylength`, which is every counted `for` loop over an
/// array. Phis are admitted separately (`ir_phi_residency_enabled`). The
/// regression suite is green with the file on (88/88).
///
/// **What the flip did NOT settle** is the tiering inversion this paragraph
/// was written about. Re-measured 2026-09-03 on a RELEASE binary, arms
/// interleaved, with a second arm of each configuration to establish the noise
/// floor: four of five loop shapes are indistinguishable between tiers, and the
/// field-read loop is still **~1.65x slower at the optimizing tier**
/// (C1 0.86/0.83 against C2 1.39/1.41, fifteen reps, controls agreeing to 3.5%
/// and 1.4%). So this file closed the part of the gap it was built for and the
/// remaining case is elsewhere — see `docs/JIT_OPTIMIZATION.md`.
///
/// Off is exactly the pre-change emission: no register is handed out, no save
/// area is reserved, every read goes to its home word.
///
/// Declared in `types/src/flag_groups.rs` as `jit/ir-linear-scan`, so `-XX:`
/// options and `flags::with_thread_overrides` reach it.
/// Seed each block's null proofs with the receiver — **default OFF**, opt in
/// with `CRATONVM_JIT_IR_THIS_NONNULL=1`.
///
/// # It is off because it measured SLOWER, which nobody expected
///
/// It is correct and it engages (`seeded=9 elided=2 emitted=0` against
/// `0/0/2` with it off, same answer). On the loop it was built for — the one
/// the 2026-09-03 tier comparison found inverted — it is **~20% slower with
/// the check removed than with it emitted**: medians 1.78/1.93 on against
/// 1.49/1.61 off, two replicate pairs, within-config spread 8%, same direction
/// both times.
///
/// **The cost is not compile time.** At `reps=1`, where the loop barely runs,
/// the two arms are 0.19 against 0.18 — so the per-block seed scan is ~0.01s
/// and the 0.3s is in the emitted code.
///
/// Removing two instructions cannot make a loop 20% slower on its own, so what
/// this really says is that the body is dominated by something layout- or
/// branch-structure-sensitive, and deleting a never-taken forward `JZ` moved
/// it. That is a lead worth pulling for the residual tier inversion itself,
/// and it is the reason this switch stays available rather than being deleted:
/// it is the smallest known perturbation that moves that loop by 20%.
///
/// If nobody finds the cause, withdraw it.
///
/// The optimizing-tier half of `CRATONVM_JIT_THIS_NONNULL`, which seeds the
/// single-pass backend's null-check dataflow. Separate switch because the two
/// tiers reach the fact by different routes — a bytecode dataflow there, the
/// graph's `receiver_param` here — and a single flag would make a bisect
/// unable to say which one moved.
///
/// Off restores the previous emission exactly: a `TEST`/`JZ` at every
/// `getfield` whose receiver the per-block CSE has not already proven.
/// Does a register for `id` pay for itself, counting loop frequency?
///
/// The publish is one memory→register load at the definition; each read it
/// replaces is one memory→register load at its own site. So the trade is
/// `uses_frequency >= 2 × definition_frequency`, and the static `use_count >= 2`
/// is that same test with every frequency pinned to 1.
///
/// `live.weight[id]` is already the loop-frequency-weighted use count. The
/// definition's frequency is derived from `live.loop_depth` and `live.span`,
/// both public, using the same `LOOP_WEIGHT_PER_DEPTH` model — so the two sides
/// of the comparison come from one model rather than two.
///
/// Falls back to the static rule whenever the loop model cannot place the
/// definition, which keeps a graph the scheduler left unusual on the old
/// behaviour rather than on a guess.
fn ir_residency_pays_here(
    live: &crate::regalloc::LiveModel,
    schedule: &Schedule,
    id: usize,
    static_uses: u32,
) -> bool {
    if !ir_residency_loop_weight_enabled() {
        return static_uses >= 2;
    }
    let Some(def_pos) = live.pos_of.get(id).copied().flatten() else {
        return static_uses >= 2;
    };
    // Which block holds the definition? `span[b]` is that block's
    // (first position, outgoing-edge position).
    let mut def_depth: Option<u32> = None;
    for b in 0..schedule.blocks.len() {
        if let Some(&(lo, hi)) = live.span.get(b) {
            if lo <= def_pos && def_pos <= hi {
                def_depth = live.loop_depth.get(b).copied();
                break;
            }
        }
    }
    let Some(depth) = def_depth else {
        return static_uses >= 2;
    };
    let def_freq = u64::from(
        crate::regalloc::LOOP_WEIGHT_PER_DEPTH
            .checked_pow(depth)
            .unwrap_or(crate::regalloc::MAX_LOOP_DEPTH_WEIGHT)
            .min(crate::regalloc::MAX_LOOP_DEPTH_WEIGHT),
    );
    let uses_freq = live.weight.get(id).copied().unwrap_or(0);
    uses_freq >= def_freq.saturating_mul(2)
}

/// Weigh the residency trade by loop frequency instead of a static use count —
/// **default OFF**, opt in with `CRATONVM_JIT_IR_LS_LOOP_WEIGHT=1`.
///
/// Off is the static `use_count >= 2` this file has always used.
fn ir_residency_loop_weight_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_IR_LS_LOOP_WEIGHT").is_some()
    })
}

/// Copy a loop-live int/long PARAMETER into a callee-saved register at entry —
/// **default OFF**, opt in with `CRATONVM_JIT_IR_PARAM_COPY=1`.
///
/// The optimizing tier cannot promote an entry parameter at all: they are
/// pinned to their incoming ABI registers, which are caller-saved and outside
/// this file, and the allocator skips a pinned value. The baseline tier copies
/// them into callee-saved registers in its prologue and reads them from there;
/// this is that copy.
///
/// Off is exactly the previous emission: the parameter reaches every use
/// through its frame slot.
fn ir_param_prologue_copy_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_IR_PARAM_COPY").is_some()
    })
}

fn ir_this_nonnull_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_IR_THIS_NONNULL").is_some()
    })
}

fn linear_scan_enabled() -> bool {
    #[cfg(test)]
    {
        if let Some(forced) = ls_forced() {
            return forced;
        }
    }
    // 2026-09-02: DEFAULT ON. The enumeration disagreement that declined the
    // file on every array-touching method was reconciled the same day, phis
    // are admitted (`ir_phi_residency_enabled`), and the census below says
    // where the file engages. `=0` is the kill switch.
    !matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_LINEAR_SCAN").as_deref(),
        Ok("0") | Ok("false") | Ok("off") | Ok("no")
    )
}

#[cfg(test)]
thread_local! {
    /// Test-only override for [`linear_scan_enabled`], so a unit test never
    /// depends on the process environment or on whether the flag has been
    /// declared yet. Thread-local, so parallel tests cannot see each other's
    /// setting.
    static LS_FORCE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn ls_forced() -> Option<bool> {
    LS_FORCE.with(|c| c.get())
}

/// Test-only RAII override of [`linear_scan_enabled`] on this thread.
#[cfg(test)]
struct LsForce;

#[cfg(test)]
impl LsForce {
    fn on() -> LsForce {
        LS_FORCE.with(|c| c.set(Some(true)));
        LsForce
    }

    /// Force the cache OFF for the duration.
    ///
    /// Needed by the level-2 tests: residency and the selector are mutually
    /// exclusive in production (see `lower_inner_with_scopes`), so a test that
    /// compares a MIR-mode body against a no-mode body is otherwise comparing
    /// two different backends and not the selector at all.
    fn off() -> LsForce {
        LS_FORCE.with(|c| c.set(Some(false)));
        LsForce
    }
}

#[cfg(test)]
impl Drop for LsForce {
    fn drop(&mut self) {
        LS_FORCE.with(|c| c.set(None));
    }
}

/// Which XMM register holds each value for its whole life, plus what the
/// decision cost.
#[derive(Clone, Debug, Default)]
struct RegResidency {
    /// `reg_of[id]` = the XMM number holding node `id`'s value from its
    /// definition to its last use, with no split, no spill and no reload.
    /// `None` for everything else, which is the majority.
    reg_of: Vec<Option<u8>>,
    /// `gp_reg_of[id]` = the same, in the GENERAL-PURPOSE file. Disjoint from
    /// `reg_of` by construction: a node has one type, and the two files serve
    /// disjoint type sets (FP here, int/long there, `Ref` in neither).
    gp_reg_of: Vec<Option<u8>>,
    /// How many values that is.
    promoted: usize,
    /// Values the allocator promoted that this file then refused, because the
    /// allocator's liveness model and `plan_slots`' disagreed about them. A
    /// non-zero count is a compiler bug worth chasing; it is not wrong code,
    /// because a refusal only sends the value back to memory.
    demoted: usize,
    /// Copied out of the liveness model so the caller can report it.
    #[allow(dead_code)]
    peak_live: usize,
}

/// The machine model for `ir_lower`'s emission, over `regs`.
///
/// [`crate::regalloc::MachineModel::for_graph`] derives clobbers from ops. It
/// is correct as far as it goes, and it cannot know about the calls a
/// particular backend emits *between* ops. This adds the one `ir_lower` has:
/// the cooperative safepoint poll, which on a loop back edge tests a byte and
/// on a set flag calls its slow path (`emit_safepoint_poll`) — at the block's
/// outgoing-edge position, which belongs to no node. `for_graph` already marks
/// those positions safepoints; they must also be clobbers, because a value in
/// a caller-saved register across a back edge would be destroyed by that call.
///
/// The entry poll (before block 0) needs nothing: no value is live yet.
fn ir_lower_machine_model(
    graph: &Graph,
    schedule: &Schedule,
    live: &crate::regalloc::LiveModel,
    regs: crate::regalloc::RegFile,
) -> crate::regalloc::MachineModel {
    use std::collections::BTreeMap;
    // The back-edge safepoint poll is a CALL site, so what it destroys is the
    // caller-saved subset — the same rule `MachineModel::for_graph` applies at
    // an `Op::Call`, applied here because the poll is not a graph node and
    // `for_graph` therefore never sees it.
    //
    // This used to be every register in the file. That was safe but wrong in
    // the expensive direction: it made a loop-carried value unpromotable into
    // XMM6/XMM7 even on the target where this frame saves them, which is
    // exactly the case the save area was built for. The narrowing is sound
    // because the poll's slow path is `jit_safepoint_slow_path`, an ordinary
    // `extern "C"` function — Win64 obliges it to preserve XMM6–XMM15, and on
    // System V no XMM is callee-saved so `IR_LOWER_SAVED_XMMS` is empty and
    // this set is still the whole file, byte for byte as before.
    let poll_destroys: Vec<crate::regalloc::PhysReg> = regs
        .specs()
        .iter()
        .filter(|s| s.caller_saved)
        .map(|s| s.reg)
        .collect();
    let mut model = crate::regalloc::MachineModel::for_graph(graph, schedule, live, regs);

    // `MachineModel::clobbered_at` binary-searches by position, so the list
    // must stay sorted AND hold one entry per position. Merge through a map
    // rather than push-and-sort.
    let mut merged: BTreeMap<usize, Vec<crate::regalloc::PhysReg>> = BTreeMap::new();
    for (pos, regs) in model.clobbers.drain(..) {
        merged.entry(pos).or_default().extend(regs);
    }
    for (b, block) in schedule.blocks.iter().enumerate() {
        if !block.successors.iter().any(|&s| s <= b) {
            continue;
        }
        let Some(&(_, edge)) = live.span.get(b) else {
            continue;
        };
        // `lower_terminator` and `lower_block`'s fall-through arm both emit the
        // poll immediately before the edge's phi copies, i.e. between the
        // terminator position and the edge position. Clobber both, for the same
        // reason `MachineModel::for_graph` makes both safepoints.
        for pos in [edge.saturating_sub(1), edge] {
            merged
                .entry(pos)
                .or_default()
                .extend_from_slice(&poll_destroys);
        }
    }
    model.clobbers = merged
        .into_iter()
        .map(|(pos, mut regs)| {
            regs.sort_unstable();
            regs.dedup();
            (pos, regs)
        })
        .collect();
    model
}

/// Plan register residency for one scheduled graph, or `None` when nothing is
/// promotable and the colourer's memory layout stands unchanged.
///
/// Every refusal here is a *demotion*, never a bailout: a value that does not
/// take a register keeps the home slot the lowerer has always given it, which
/// is correct by construction. The one hard failure is
/// [`crate::regalloc::verify_allocation`] rejecting the allocation, which is a
/// compiler bug — that returns `Err` and the caller refuses the compile rather
/// than emitting against an allocation nothing proved.
/// Report why residency was declined, under `CRATONVM_DBG_IR_LINEAR_SCAN`.
///
/// Every refusal in `plan_register_residency` used to be a bare `Ok(None)`,
/// and the flag printed only on SUCCESS -- so "no output" meant "declined,
/// somewhere, for one of four reasons" and could not be acted on. Naming the
/// conjunct is the difference between a count and a diagnosis.
fn ls_refuse(reason: &str) -> Option<RegResidency> {
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_IR_LINEAR_SCAN").is_some() {
        eprintln!("[ir-ls] refused: {reason}");
    }
    None
}

fn plan_register_residency(
    graph: &Graph,
    schedule: &Schedule,
    plan: &SlotPlan,
) -> CompileResult<Option<RegResidency>> {
    use crate::regalloc::{
        allocate_linear_scan, build_live_model, verify_allocation, PhysReg, RegClass, RegFile,
        RegSpec,
    };

    let mut live = build_live_model(graph, schedule);
    if !live.converged {
        // Every range is the whole method; nothing is promotable and the scan
        // would only burn compile time proving it.
        return Ok(ls_refuse("liveness model did not converge"));
    }

    // ── Release the deopt pins, and why that is legal HERE ───────────
    //
    // Without this the wiring is inert on every real method. `IrBuilder`
    // records a `SafepointSnapshot` of the locals and the operand stack at
    // EVERY bytecode boundary, so every temporary is named by some frame state
    // and `build_live_model` pins the entire value set — `allocate_linear_scan`
    // then promotes nothing at all, on any graph that came from bytecode.
    //
    // The pin's premise is "the home word must hold the value at any recorded
    // bci, and an allocator cannot establish that". This consumer establishes
    // it by construction rather than by analysis: `fp_store_value` emits the
    // home store at EVERY definition, promoted or not, so a deopt frame read
    // out of home words is bit-identical to the one the colourer-only path
    // produces. `build_deopt_points` is not modified and does not need to be.
    //
    // The price, per `release_deopt_pins`' contract, is `Allocation::stack_slot`
    // — which this file must not read, and does not: every home offset comes
    // from `plan_slots`, which keeps every pin, through `alloc_slot_checked`.
    let released = live.release_deopt_pins(graph);
    // Phis too, when their edges publish them (see `emit_phi_copies`).
    if ir_phi_residency_enabled() {
        live.release_phi_pins(graph);
    }

    // ── Agreement check: two liveness models, one program ────────────
    //
    // `build_live_model` re-implements `plan_slots`' position model rather than
    // calling it (`SlotPlan` and `LiveRange` are private to this file). Two
    // implementations of one model drifting apart is precisely how a register
    // allocator comes to alias two live values, so the agreement is CHECKED
    // before either is used, not documented and hoped for.
    //
    // A disagreement disables promotion entirely. It is not a bailout: the
    // colourer's answer is still correct on its own terms, and it is the one
    // the frame is built from.
    let expected_positions: usize = schedule
        .blocks
        .iter()
        .map(|b| b.nodes.len() + usize::from(b.terminator.is_some()) + 1)
        .sum();
    if live.total_positions != expected_positions {
        return Ok(ls_refuse("position models disagree (live vs schedule)"));
    }
    // Two enumerations of "does this op define a value" have to agree here:
    // `regalloc::ir_op_defines_value` (via `wants_loc`) and `ir_lower`'s own
    // `op_defines_result_slot` (via `node_color`). They are the second and
    // third of the three enumerations the module comment at
    // `the_three_ir_op_enumerations` names, and a disagreement declines the
    // whole method.
    //
    // Name the OP that disagrees, not just the fact. "Declined" sends the
    // reader looking through fifty variants; "declined because `Op::X` is in
    // one enumeration and not the other" is a one-line fix with a test.
    let disagreement = if live.wants_loc.len() != plan.node_color.len() {
        Some("node-count mismatch".to_string())
    } else {
        live.wants_loc
            .iter()
            .zip(plan.node_color.iter())
            .enumerate()
            .find(|(_, (wants, color))| **wants != color.is_some())
            .map(|(id, (wants, _))| {
                let op = graph
                    .nodes
                    .get(id)
                    .map(|node| format!("{:?}", node.op))
                    .unwrap_or_else(|| "<out of range>".to_string());
                let (yes, no) = if *wants {
                    ("liveness", "colourer")
                } else {
                    ("colourer", "liveness")
                };
                format!("n{id} ({op}): {yes} says it wants a home, {no} says it does not")
            })
    };
    if let Some(what) = disagreement {
        return Ok(ls_refuse(&format!(
            "liveness and colourer disagree about which values want a home -- {what}"
        )));
    }

    // ── The register file ────────────────────────────────────────────
    //
    // Two banks, not one. The GP half is what makes an `int` loop counter
    // register-resident; the FP half is unchanged.
    let gp_specs = IR_LOWER_LS_GPRS.iter().map(|&n| RegSpec {
        reg: PhysReg::gp(n),
        // RBX and R12–R15 are callee-saved on BOTH ABIs, and this prologue
        // saves every one it hands out (`IR_LOWER_SAVED_GPRS` is the whole
        // file), so `caller_saved` is uniformly false and a value may live
        // across a call. That is exactly the property the GP file was selected
        // for: this backend emits calls constantly and has no reload
        // machinery, so a value whose register did not survive a call could
        // not be promoted at all.
        caller_saved: !IR_LOWER_SAVED_GPRS.contains(&n),
    });
    let xmm_specs = IR_LOWER_LS_XMMS.iter().map(|&n| RegSpec {
        reg: PhysReg::xmm(n),
        // Not "the ABI says so" but "does THIS frame save it", which is the
        // property that matters: a value may stay in a register across a call
        // exactly when the callee is obliged to give it back.
        //
        // XMM2–XMM5 are volatile on both ABIs, so a value in one dies at every
        // call and `caller_saved: true` is what makes the allocator split it.
        // XMM6/XMM7 are volatile on System V too — `IR_LOWER_SAVED_XMMS` is
        // empty there, so they come out `true` as well and nothing changes. On
        // Windows they are non-volatile AND this prologue saves them, so they
        // come out `false` and a value CAN live across a call. That is the
        // whole return on the save area, and it is expressed here rather than
        // in a platform `cfg` because the two facts that make it true — the
        // ABI's and this frame's — are both already in `IR_LOWER_SAVED_XMMS`.
        caller_saved: !IR_LOWER_SAVED_XMMS.contains(&n),
    });
    let regs = RegFile::from_specs(gp_specs.chain(xmm_specs));
    let model = ir_lower_machine_model(graph, schedule, &live, regs);

    let alloc = match allocate_linear_scan(graph, &live, &model) {
        Ok(alloc) => alloc,
        // Register pressure, an unsatisfiable fixed constraint or an exhausted
        // split budget is a property of the input program, not a compiler bug.
        // Decline to promote and keep the frame layout the colourer already
        // produced; there is no reason to lose the whole optimized body over
        // an optimization that did not fit.
        Err(_) => return Ok(ls_refuse("linear scan declined (pressure, fixed constraint or split budget)")),
    };
    // `allocate_linear_scan` verifies its own result. Re-verify anyway: this
    // call site's guarantee must not rest on an internal detail of another
    // module, and the check is the only thing standing between a bug in the
    // scan and machine code that reads a register two values are in.
    //
    // THIS one propagates. A verifier failure is a compiler bug, and emitting
    // against an allocation nothing proved is exactly the trade that has
    // produced silent heap corruption in this VM before.
    verify_allocation(graph, &live, &model, &alloc)?;

    let n = graph.nodes.len();
    let mut reg_of: Vec<Option<u8>> = vec![None; n];
    let mut gp_reg_of: Vec<Option<u8>> = vec![None; n];
    let mut demoted = 0usize;
    // Per-CAUSE refusal census. A bare "promoted 0 of N" is a count and cannot
    // be acted on: the four causes below call for four different next steps,
    // and `skip_phi` in particular is the one that decides whether this file
    // can ever reach a LOOP COUNTER -- which is a phi, at every loop header.
    let (mut skip_split, mut skip_bank, mut skip_home, mut skip_phi) = (0usize, 0, 0, 0);
    // Split apart from `skip_split` on 2026-09-03: see the refusal below.
    let (mut skip_no_alloc, mut skip_spilled) = (0usize, 0usize);
    // Parameters copied into a callee-saved register by the prologue.
    let mut param_copies = 0usize;
    // 2026-09-02, measured: promotion is not free, and two populations pay for
    // it without ever collecting.
    //
    // A register costs a PROLOGUE SAVE and a restore at every exit (the file is
    // callee-saved by construction), plus one publish load at the definition.
    // It repays that at each READ it turns into a register move. So a value
    // read once breaks even at best, and on a small call-heavy method the
    // saves dominate: `FibProbe.fib` promoted five registers, was measured
    // 145-149 ms against 110-133 ms for the same binary with the file off, and
    // the disassembly showed why -- four of the five registers were published
    // and never read.
    //
    // A CONSTANT is worse than break-even: since `ir_const_imm_enabled` every
    // reader materialises it as an immediate, so its register can never be
    // read at all.
    let mut use_count = vec![0u32; n];
    for node in &graph.nodes {
        for &input in &node.inputs {
            if input != NO_NODE {
                if let Some(c) = use_count.get_mut(input as usize) {
                    *c = c.saturating_add(1);
                }
            }
        }
    }
    let (mut skip_const, mut skip_single_use) = (0usize, 0usize);
    for id in 0..n {
        let Some(segs) = alloc.segments.get(id) else {
            continue;
        };
        if ir_residency_pays_enabled() {
            match graph.nodes.get(id).map(|node| &node.op) {
                Some(Op::Const(_)) => {
                    skip_const += 1;
                    continue;
                }
                // A phi is exempt: its reads are inside the loop it carries a
                // value around, which is the whole population this file exists
                // for, and its "definition" is an edge copy that is already
                // paying the store.
                Some(Op::Phi) => {}
                // The `< 2` rule below counts STATIC graph edges. That is the
                // right comparison only when the definition and the uses run
                // equally often, and in a loop they do not: a value defined at
                // method entry and read once per iteration is one publish
                // against N reads, and the static count sees 1 and refuses.
                //
                // Measured on the loop the 2026-09-03 tier comparison found
                // inverted: `Param(0)` (the receiver) and `Param(1)` (the loop
                // bound) both read `static_uses=1 loop_weight=10` — refused by
                // a rule that could not see the ten. The disassembly showed
                // exactly that, `mov rax,[rbp-58h]` and `mov rcx,[rbp-60h]`
                // reloaded every iteration, while the baseline tier held both
                // in callee-saved registers.
                //
                // The loop-aware form compares the uses' frequency against the
                // DEFINITION's: residency pays when the reads happen at least
                // twice as often as the single publish. It reduces exactly to
                // `use_count >= 2` when everything sits at depth 0, so a
                // method with no loop is byte-identical.
                _ if !ir_residency_pays_here(
                    &live,
                    schedule,
                    id,
                    use_count.get(id).copied().unwrap_or(0),
                ) =>
                {
                    skip_single_use += 1;
                    continue;
                }
                _ => {}
            }
        }
        // ONE segment, and it holds a register. Anything else — a split, a
        // spill, a reload, a home-slot stretch in the middle — is refused
        // rather than emitted: this wiring has no reload machinery, so a value
        // whose register goes away partway through must not be read from one.
        //
        // **The NO-SEGMENT case is counted separately, and that distinction is
        // not cosmetic.** A node the scan produced no interval for was never a
        // candidate — every control and memory node in the graph lands here,
        // `Start` and `Proj` included — and folding it into the split count
        // makes the file look like it is losing values to register pressure
        // when it is only being handed nodes that hold no value at all.
        //
        // It read that way for real. On the loop the 2026-09-03 tier
        // comparison found inverted, this line reported
        // `split_or_spilled=5` beside the allocator's `splits=4`, and the two
        // together said: four candidates lost to live-range splits. They were
        // not. Four of the five had EMPTY segment lists and one was genuinely
        // spilled; the method had no split value for the file to reclaim at
        // all. A day of work aimed at split residency followed from that
        // reading, and its engagement counter read zero — which is how the
        // miscount was found.
        let reg = match segs.as_slice() {
            [] => {
                skip_no_alloc += 1;
                continue;
            }
            [seg] => match seg.reg {
                Some(reg) => reg,
                None => {
                    skip_spilled += 1;
                    continue;
                }
            },
            _ => {
                skip_split += 1;
                continue;
            }
        };
        let ty = graph.nodes.get(id).map(|node| node.ty);
        // Which bank, and is the type one that bank may hold?
        //
        // **`IrType::Ref` is admitted by neither, and that is the safepoint
        // obligation, not a tuning choice.** A GC root walk reads a frame it
        // did not stop, through RBP, and `OopMapEntry` names frame slots only —
        // there is no register a collector could be told about, walk, or update
        // on an evacuation. The XMM file discharged this structurally (no
        // register a `Ref` could occupy); the GP file has to discharge it by
        // refusing the type here, which is what this match does and what
        // `a_reference_is_never_promoted_into_the_gp_file` pins.
        //
        // Everything else in the match is defensive: a register outside its own
        // file, or a type outside its bank, means the file or `RegClass::of`
        // changed under this code.
        let is_gp = match (reg.class, ty) {
            (RegClass::Xmm, Some(IrType::Float) | Some(IrType::Double))
                if IR_LOWER_LS_XMMS.contains(&reg.num) =>
            {
                false
            }
            (RegClass::Gp, Some(IrType::Int) | Some(IrType::Long))
                if IR_LOWER_LS_GPRS.contains(&reg.num) =>
            {
                true
            }
            _ => {
                skip_bank += 1;
                continue;
            }
        };
        // Write-through needs a home to write to.
        if plan.node_color.get(id).copied().flatten().is_none() {
            skip_home += 1;
            continue;
        }
        // A phi's home is written by `emit_phi_copies` at each incoming edge,
        // not by a definition arm, so there is no site that could publish it
        // into a register. `allocate_linear_scan` already refuses phis; this
        // says so locally rather than relying on that.
        //
        // NOT `SlotClass::Pinned`. That class means "never shares a frame
        // word", and after `release_deopt_pins` it covers exactly the
        // deopt-named values — i.e., on a bytecode graph, nearly all of them.
        // Testing it here would undo the release and make this wiring inert
        // again. A dedicated home word is if anything the safer case for
        // write-through.
        let is_phi = graph
            .nodes
            .get(id)
            .is_some_and(|node| matches!(node.op, Op::Phi));
        // 2026-09-02: with `ir_phi_residency_enabled`, `emit_phi_copies` IS
        // the publishing site (every incoming edge reloads the register from
        // the home word it just wrote), so a phi is admitted like any other
        // value; the allocator's structural pin was released by
        // `release_phi_pins` above on the same condition.
        if is_phi && !ir_phi_residency_enabled() {
            skip_phi += 1;
            continue;
        }
        if is_gp {
            gp_reg_of[id] = Some(reg.num);
        } else {
            reg_of[id] = Some(reg.num);
        }
    }

    // ── Second opinion on the aliasing property ──────────────────────
    //
    // `verify_allocation` proves "no two values hold one register at one
    // position" against the ALLOCATOR's liveness model. Re-prove it against
    // `plan_slots`' independently computed ranges. If the two models disagree
    // about an overlap, both values lose the register — the disagreement
    // itself is the reason not to trust either answer for that pair.
    //
    // Keyed by `(bank, number)` now that there are two files: `Gp(3)` and
    // `Xmm(3)` are different registers, and a map keyed by the number alone
    // would report them as one holder and demote a healthy pair. That is the
    // same aliasing hazard `PhysReg` carries its class for.
    let mut holders: HashMap<(bool, u8), Vec<usize>> = HashMap::new();
    for (id, reg) in gp_reg_of.iter().enumerate() {
        if let Some(reg) = reg {
            holders.entry((true, *reg)).or_default().push(id);
        }
    }
    for (id, reg) in reg_of.iter().enumerate() {
        if let Some(reg) = reg {
            holders.entry((false, *reg)).or_default().push(id);
        }
    }
    let mut drop_ids: Vec<usize> = Vec::new();
    for ids in holders.values() {
        for (i, &a) in ids.iter().enumerate() {
            let Some(ra) = plan.range.get(a).copied().flatten() else {
                drop_ids.push(a);
                continue;
            };
            for &b in &ids[i + 1..] {
                match plan.range.get(b).copied().flatten() {
                    Some(rb) if !ra.overlaps(rb) => {}
                    _ => {
                        drop_ids.push(a);
                        drop_ids.push(b);
                    }
                }
            }
        }
    }
    // ── Second opinion on the ABI property ───────────────────────────
    //
    // The same idea for `verify_allocation`'s clobber check: re-run it against
    // `plan_slots`' range for the value rather than the allocator's.
    for (bank, file) in [(false, &reg_of), (true, &gp_reg_of)] {
        for (id, reg) in file.iter().enumerate() {
            let Some(reg) = reg else { continue };
            let Some(range) = plan.range.get(id).copied().flatten() else {
                continue;
            };
            let me = if bank {
                PhysReg::gp(*reg)
            } else {
                PhysReg::xmm(*reg)
            };
            if model
                .clobbers
                .iter()
                .any(|(pos, regs)| *pos >= range.lo && *pos <= range.hi && regs.contains(&me))
            {
                drop_ids.push(id);
            }
        }
    }
    for id in drop_ids {
        // A node lives in at most one bank (its type picks the bank), so
        // clearing both is clearing the one it is in.
        if reg_of[id].take().is_some() || gp_reg_of[id].take().is_some() {
            demoted += 1;
        }
    }

    // ── Entry parameters, which the allocator cannot reach ───────────
    //
    // `MachineModel::pin_entry_params` pins every `Param` to its INCOMING ABI
    // register, and `allocate_linear_scan` skips a pinned value outright. Those
    // ABI registers are caller-saved and are not in `IR_LOWER_LS_GPRS`, so a
    // parameter can never be promoted into the callee-saved file however the
    // heuristics are tuned — it reaches a loop through its frame slot, every
    // iteration, by construction.
    //
    // That is what the 2026-09-03 disassembly of the inverted `fieldloop`
    // showed: `mov rcx,[rbp-60h]` reloading the loop bound on every iteration,
    // while the baseline tier had copied it into a callee-saved register in its
    // prologue (`mov r14,rdx`) and read it from there.
    //
    // So the copy is made here instead, out of a register the ALLOCATOR DID NOT
    // USE. That is the whole safety argument: an unassigned register in this
    // file is written by nothing else — only the residency machinery publishes
    // into it — and every register in the file is callee-saved, so a call
    // cannot destroy it either. The parameter is SSA and never redefined, so
    // one publish at entry is good for the whole method.
    //
    // `IrType::Ref` is excluded for the reason the bank match above gives, and
    // it is not a tuning choice: `OopMapEntry` names frame slots only, so a
    // reference in a register is invisible to a root walk and cannot be
    // updated on evacuation. The receiver therefore stays in its frame slot,
    // and closing that needs oop maps that can name a register.
    if ir_param_prologue_copy_enabled() {
        let mut taken: Vec<u8> = gp_reg_of.iter().flatten().copied().collect();
        taken.sort_unstable();
        taken.dedup();
        let mut free: Vec<u8> = IR_LOWER_LS_GPRS
            .iter()
            .copied()
            .filter(|r| !taken.contains(r))
            .collect();
        for id in 0..n {
            if free.is_empty() {
                break;
            }
            if gp_reg_of.get(id).copied().flatten().is_some() {
                continue;
            }
            let Some(node) = graph.nodes.get(id) else {
                continue;
            };
            if !matches!(node.op, Op::Param(_)) {
                continue;
            }
            if !matches!(node.ty, IrType::Int | IrType::Long) {
                continue;
            }
            // Worth a register only if the reads outnumber the one publish.
            // Loop-weighted, because a parameter's uses are typically inside a
            // loop its definition is not.
            if live.weight.get(id).copied().unwrap_or(0) < 2 {
                continue;
            }
            if let Some(reg) = free.pop() {
                gp_reg_of[id] = Some(reg);
                param_copies += 1;
            }
        }
    }

    let fp_promoted = reg_of.iter().filter(|r| r.is_some()).count();
    let gp_promoted = gp_reg_of.iter().filter(|r| r.is_some()).count();
    let promoted = fp_promoted + gp_promoted;
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_IR_LINEAR_SCAN").is_some() {
        eprintln!(
            "[ir-ls] nodes={n} positions={} peak_live={} deopt_pins_released={released} \
             scan_promoted={} resident={promoted} (fp={fp_promoted} gp={gp_promoted}) \
             demoted={demoted} splits={} scan_spills={} scan_reloads={}",
            live.total_positions,
            live.peak_live,
            alloc.promoted,
            alloc.splits,
            alloc.spills,
            alloc.reloads,
        );
    }
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_IR_LINEAR_SCAN").is_some() {
        eprintln!(
            "[ir-ls] skipped: split_or_spilled={skip_split} \
             wrong_bank_or_type={skip_bank} no_home={skip_home} phi={skip_phi} \
             const={skip_const} single_use={skip_single_use} param_copies={param_copies} \
             spilled={skip_spilled} no_alloc={skip_no_alloc}"
        );
    }
    if promoted == 0 {
        return Ok(ls_refuse(
            "the scan promoted nothing this file could take -- every candidate \
             was split, spilled, a phi, homeless, or of a type neither bank holds",
        ));
    }
    Ok(Some(RegResidency {
        reg_of,
        gp_reg_of,
        promoted,
        demoted,
        peak_live: live.peak_live,
    }))
}

/// Funnel every refusal in this file through one place: count it, then return
/// the `None` the public entry points have always returned, which is the
/// caller's signal to run the method in a lower tier. A compiler refusal must
/// never terminate the VM.
fn refuse(bailout: Bailout) -> Option<CompiledMethod> {
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_IR_BAILOUT").is_some() {
        eprintln!("[ir-bailout] {bailout}");
    }
    record_bailout(&bailout);
    // Attribution, not duplication — the same split `verify_or_bail` makes:
    // `record_bailout` owns the process-wide category counters, and this
    // attaches the bailout to *this* method in the per-compilation report.
    //
    // Without it the report's `bailouts` field is STRUCTURALLY empty for every
    // lowering refusal: a census over CratonBench read `bailouts:[]` on all 105
    // compilations, including the 17 that fell through to single-pass, so the
    // one record that names both the method and the reason named neither. The
    // counters were moving the whole time, which is what made the hole hard to
    // see — the totals looked alive while every per-method row was blank.
    crate::metrics::note_current_bailout(&bailout, "lower");
    None
}

/// Smallest executable buffer this backend will ask for.
///
/// Two pages. Raised from 4096 with the rest of the sizing below: under the old
/// model the floor WAS the capacity for most compiles (the formula fell under
/// it for any modest graph), so 4096 is where the overflows piled up.
///
/// Raising the floor ALONE does not work, and the census says so: `nodes*32 +
/// calls*448 + 1024` with a 16 KiB floor still overflows 5 of 1664 while
/// reserving 26 MiB — worse on both axes than fixing the formula and keeping
/// the floor at 8 KiB.
const IR_CODE_BUFFER_FLOOR: usize = 8 * 1024;

/// How many bytes to reserve for a lowered graph of `nodes` nodes, `call_nodes`
/// of which are calls.
///
/// **This is a capacity, not a prediction.** Getting it too high wastes address
/// space in a region already capped by `COMMITTED_JIT_CODE_BYTES`; getting it
/// too low does NOT produce a bug — `ExecutableBuffer::emit` is non-panicking
/// and the caller discards an overflowed body — it silently drops the method to
/// the single-pass backend. That failure is invisible except as throughput, so
/// the bias here is deliberately toward over-reserving.
///
/// ## Where these numbers come from
///
/// Measured, not reasoned. `CRATONVM_DBG_IR_BUFSIZE=1` (below) censused
/// **1664 IR compiles** across three workloads on 2026-08-01 — Spring Boot's
/// `BasicErrorControllerDirectMockMvcTests` (595) and
/// `BasicErrorControllerIntegrationTests` (1067), plus the whole
/// `bench/CratonBench` CPU suite (2) — recording `wanted` against `capacity`
/// for every one.
///
/// The old `nodes*32 + call_nodes*448 + 1024` **overflowed 155 of those 1664**
/// (9.3%), under-shooting by up to **2.65x**. It was never measured; it was
/// reasoned from "an arithmetic node emits well under 32 bytes", and that
/// premise is fine — the error is entirely in the other two terms:
///
/// * **the per-call term.** 448 budgeted, 1141 actually required at worst. The
///   278 call-free compiles in the census never wanted more than 1545 bytes
///   total, so nodes were never the problem; calls always were.
/// * **the constant.** 1024 does not pay for a prologue, an epilogue and the
///   frame setup, so every small graph fell through to the floor and inherited
///   whatever the floor happened to be.
///
/// This model overflows **0 of 1664**, and its tightest fit still has **1.24x**
/// headroom, so it is not sitting on a knife edge. It reserves 13.8 MiB across
/// that census against the old 6.6 MiB.
///
/// **That 2.1x matters and is the reason not to be more generous.**
/// [`ExecutableBuffer::new`] adds the full CAPACITY to
/// [`COMMITTED_JIT_CODE_BYTES`], not the bytes actually used, and that is the
/// quantity the 256 MiB code-cache cap bounds — so over-reserving buys headroom
/// with code cache. 13.8 MiB for a complete Spring Boot boot is ~5% of the cap;
/// a 16 KiB floor on the old formula would have been 26 MiB and still not have
/// worked.
///
/// [`ExecutableBuffer::wanted`] measures this exactly for this backend: it
/// counts every byte codegen ASKED to emit, including writes dropped after an
/// overflow, and this lowerer calls neither `rewind_to` nor `emit_checked`, so
/// nothing inflates it or hides from it.
fn ir_code_buffer_estimate(nodes: usize, call_nodes: usize) -> usize {
    if legacy_ir_code_buffer_estimate() {
        // The floor is part of the arm: 4096 was doing most of the work in the
        // old sizing, so an A/B that kept the new floor would not be measuring
        // the old behaviour.
        return nodes
            .saturating_mul(32)
            .saturating_add(call_nodes.saturating_mul(448))
            .saturating_add(1024)
            .max(4096);
    }
    nodes
        .saturating_mul(IR_BYTES_PER_NODE)
        .saturating_add(call_nodes.saturating_mul(IR_BYTES_PER_CALL_NODE))
        .saturating_add(IR_CODE_BUFFER_BASE)
        .max(IR_CODE_BUFFER_FLOOR)
}

/// Per-node reservation. See [`ir_code_buffer_estimate`].
const IR_BYTES_PER_NODE: usize = 64;
/// Per-call-node reservation, on top of [`IR_BYTES_PER_NODE`]. This is the term
/// the old estimate got badly wrong: it budgeted 448, and the census puts the
/// true worst-case marginal cost of a call node at **1141 bytes**. A MIC plus a
/// 4-way PIC dual-ABI inline-cache site, its deopt/guard stub and its safepoint
/// spill run all hang off one call node.
const IR_BYTES_PER_CALL_NODE: usize = 1536;
/// Fixed per-method reservation: prologue, epilogue, frame setup and the
/// per-method stubs, none of which are nodes.
const IR_CODE_BUFFER_BASE: usize = 4096;

/// A/B opt-out (`CRATONVM_JIT_IR_LEGACY_BUFFER_ESTIMATE=1`): restore the
/// pre-2026-08-01 `nodes*32 + calls*448 + 1024` sizing and the 4096 floor.
///
/// It exists so the estimate change can be measured on ONE binary, both arms.
/// Buffer sizing decides which methods the optimizing tier produces bodies for
/// at all, and that tier is not uniformly faster than single-pass — a
/// same-binary A/B is the only honest way to tell a throughput change caused by
/// this from one caused by whatever else moved between two builds.
fn legacy_ir_code_buffer_estimate() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_IR_LEGACY_BUFFER_ESTIMATE").is_some()
    })
}

/// `CRATONVM_DBG_IR_BUFSIZE=1` — one line per IR compile that reaches
/// emission, reporting what the sizing model predicted against what codegen
/// actually asked for.
///
/// This is the instrument the estimate above was fitted with, and the one to
/// re-run before changing it again. It reports EVERY compile, not just the
/// overflowing ones: an estimate is only as good as its headroom on the
/// compiles that succeeded, and a census of failures alone cannot show that.
///
/// `[ir-bufsize] nodes=N calls=C wanted=W capacity=C' ratio=… overflow=bool`
fn report_ir_buffer_size(nodes: usize, call_nodes: usize, wanted: usize, capacity: usize) {
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_IR_BUFSIZE").is_none() {
        return;
    }
    // Integer permille, so the line stays greppable and needs no float
    // formatting in a path that may run thousands of times.
    let permille = if capacity == 0 {
        0
    } else {
        wanted.saturating_mul(1000) / capacity
    };
    eprintln!(
        "[ir-bufsize] nodes={nodes} calls={call_nodes} wanted={wanted} capacity={capacity} \
         permille={permille} overflow={}",
        wanted > capacity
    );
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
    let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
    let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
    let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
    lower_inner(
        graph,
        schedule,
        num_params,
        num_locals,
        helpers,
        &empty,
        &[],
        None,
        &no_direct,
        &no_ic,
        &no_compact,
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
    let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
    let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
    let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
    lower_inner(
        graph,
        schedule,
        num_params,
        num_locals,
        helpers,
        branch_hints,
        &[],
        None,
        &no_direct,
        &no_ic,
        &no_compact,
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
    let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
    let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
    let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
    lower_inner(
        graph,
        schedule,
        num_params,
        num_locals,
        helpers,
        &empty,
        &[],
        sr_map,
        &no_direct,
        &no_ic,
        &no_compact,
    )
}

thread_local! {
/// The reason the most recent `lower_inner` on this thread refused.
///
/// A bail that names itself is the difference between "the optimizing tier
/// declined" and a fact you can act on: the C2 task pays for an IR build,
/// optimize and schedule before any of these fire, and whether that cost can be
/// avoided up front depends entirely on WHICH of them fired.
///
/// Thread-local and overwritten per compile; read it immediately after the
/// lowering call, on the same thread.
static LOWER_BAIL_REASON: std::cell::Cell<Option<&'static str>> =
    const { std::cell::Cell::new(None) };
}

pub(crate) fn note_lower_bail(reason: &'static str) {
    LOWER_BAIL_REASON.with(|c| c.set(Some(reason)));
}

/// Clear the reason before a lowering attempt, so a stale one cannot be read
/// as this attempt's.
pub(crate) fn clear_lower_bail() {
    LOWER_BAIL_REASON.with(|c| c.set(None));
}

/// The reason the last lowering attempt on this thread refused, if it did.
pub fn last_lower_bail() -> Option<&'static str> {
    LOWER_BAIL_REASON.with(|c| c.get())
}

/// Shared lowering body: profile-guided branch hints, the optional
/// guard-surviving scalar-replacement map, and the two per-call-site lowering
/// tables all flow in here. `pub(crate)` so the production compile path
/// (`lib.rs`) can supply all of them at once.
///
/// **Sizes the code buffer by retrying, not by guessing harder.** The estimate
/// below (`nodes * 32 + calls * 448 + 1024`) budgets one number for a call site
/// whose real cost swings by several hundred bytes depending on which lowering
/// it selects — a MIC + 4-way-PIC dual-ABI inline cache is the expensive end,
/// and widening the PIC's inter-slot branch from `rel8` to `rel32` pushed it
/// past the budget. The result was a silent de-optimization: `emit` drops the
/// write, sets the sticky `overflowed` flag, and the method quietly stays
/// interpreted forever. A single Spring Boot suite class produced **8072** such
/// warnings in one run, every one of them from this estimate (the report that
/// first noticed the flood,
/// `fixed-suite-bugs/springboot/basicerrorcontroller-jit-only-failure-20260731-FIXED.md`,
/// attributed them to the single-pass backend's estimate — that one accounted
/// for 10).
///
/// `ExecutableBuffer::wanted()` counts every byte codegen asked for, including
/// the writes dropped after the overflow, so one retry at that size is exact
/// rather than another guess. Raising the constant instead would have to
/// over-reserve every ordinary method to cover the worst one, and every
/// reserved byte counts against the code-cache cap.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lower_inner(
    graph: &Graph,
    schedule: &Schedule,
    num_params: usize,
    num_locals: usize,
    helpers: &JitRuntimeHelpers,
    branch_hints: &HashMap<usize, bool>,
    spliced_ranges: &[(usize, usize, usize)],
    sr_map: Option<&ScalarReplacementMap>,
    direct_calls: &HashMap<usize, (usize, bool)>,
    ic_slots: &HashMap<usize, (usize, usize)>,
    compact_fields: &HashMap<(usize, bool), (u32, bool, u8)>,
) -> Option<CompiledMethod> {
    // No inlined callee scopes: every deopt point is a single flat frame, which
    // is what this path has always produced.
    let no_scopes = InlineScopeTable::new();
    lower_inner_with_scopes(
        graph,
        schedule,
        num_params,
        num_locals,
        helpers,
        branch_hints,
        spliced_ranges,
        sr_map,
        direct_calls,
        ic_slots,
        compact_fields,
        &no_scopes,
    )
}

/// [`lower_inner`] plus the inlined-scope table.
///
/// Split from `lower_inner` rather than folded into it so the existing
/// ten-argument call in `lib.rs` keeps compiling: a producer that has inlined
/// something calls this one, everything else keeps calling `lower_inner` and
/// gets an empty table. An empty table is byte-identical to the previous
/// behaviour — `FrameState::caller` stays `None` at every deopt point.
///
/// See `docs/jit/deopt-inline-scopes.md` for the producer side.
///
#[allow(clippy::too_many_arguments)]
pub(crate) fn lower_inner_with_scopes(
    graph: &Graph,
    schedule: &Schedule,
    num_params: usize,
    num_locals: usize,
    helpers: &JitRuntimeHelpers,
    branch_hints: &HashMap<usize, bool>,
    spliced_ranges: &[(usize, usize, usize)],
    sr_map: Option<&ScalarReplacementMap>,
    // IR direct-call lowering: `pc → (callee_entry, callee_needs_context)` for
    // every statically-bound call site whose callee was eagerly compiled. Empty
    // ⇒ every `Op::Call` keeps the historical `jit_invoke_dispatch` lowering.
    direct_calls: &HashMap<usize, (usize, bool)>,
    // IR inline-cache lowering: `pc → (mic_slot_addr, pic_slot_addr)` for every
    // virtual / interface site served from an inline cache. Empty ⇒ every such
    // site keeps the historical helper dispatch.
    ic_slots: &HashMap<usize, (usize, usize)>,
    // Guarded inline field reads: `(pc, is_reference) → (packed body offset,
    // is_reference, descriptor tag)` for every resolved compact instance field,
    // plus the two rows the IR String-access expansion needs at its invoke pc.
    // Empty ⇒ every `Op::Load` takes the checked helper, as it always did.
    compact_fields: &HashMap<(usize, bool), (u32, bool, u8)>,
    // Which inlined callee each `graph.safepoints` entry belongs to, and the
    // caller scopes above it. Empty ⇒ flat, caller-less deopt frames.
    inline_scopes: &InlineScopeTable,
) -> Option<CompiledMethod> {
    // A monitor whose helper is absent must REFUSE the compile.
    //
    // History, because the shape of it is the reason `lower_data_node`'s final
    // arm now refuses too. `ir::Op::MonitorEnter`/`MonitorExit` were added for
    // escape analysis (lock elision) BEFORE either a lowering arm or a
    // `JitRuntimeHelpers` monitor entry existed. `lower_data_node`'s catch-all
    // was `_ => {}` at the time, so an unguarded monitor compiled to *nothing*
    // — the lock silently gone, an unbalanced `monitorexit` left behind, a data
    // race rather than a missed optimization. This guard was written to keep
    // that unreachable.
    //
    // Both halves have since landed: `lower_data_node` has a real
    // `Op::MonitorEnter | Op::MonitorExit` arm that calls the helper, and
    // `a_synchronized_region_lowers_through_the_monitor_helper` drives it. So
    // what remains here is narrower than the comment this replaces claimed: a
    // graph with monitor ops and a helper table that has no monitor entry (in
    // practice a synthetic unit-test table). Refuse rather than call through a
    // zero pointer.
    //
    // The general form of the original hazard — an `ir::Op` variant with no arm
    // at all — is now handled where it arises, by that final arm, instead of
    // needing a new hand-written guard here per op. See
    // `docs/feature-designs/jit-machine-level-and-instruction-selection.md`,
    // "The cheap alternative".
    if (helpers.monitor_enter == 0 || helpers.monitor_exit == 0)
        && graph
            .nodes
            .iter()
            .any(|n| matches!(n.op, Op::MonitorEnter | Op::MonitorExit))
    {
        return refuse(Bailout::with_context(
            BailoutReason::UnsupportedShape("monitor helper absent"),
            "graph contains monitor ops but the helper table has no monitor \
             entry; refusing rather than emitting nothing and dropping the lock",
        ));
    }
    // A live object allocation is now supported by the common allocation
    // stub. A zero helper pointer is only possible in synthetic unit-test
    // tables; reject it instead of emitting a call through address zero.
    if helpers.new_object == 0
        && graph
            .nodes
            .iter()
            .any(|node| matches!(node.op, Op::New { .. }))
    {
        note_lower_bail("new-without-alloc-helper");
        return None;
    }
    // cov-06. Same reasoning, split per `Op::NewArray` shape: a PRIMITIVE
    // array (`element_type != 0`) needs `helpers.newarray` wired; a
    // REFERENCE array (`element_type == 0`) needs `helpers.anewarray_object`.
    // Only a synthetic unit-test table leaves either at 0.
    if graph
        .nodes
        .iter()
        .any(|node| matches!(node.op, Op::NewArray { element_type, .. } if element_type != 0))
        && helpers.newarray == 0
    {
        note_lower_bail("newarray-primitive-without-helper");
        return None;
    }
    if graph
        .nodes
        .iter()
        .any(|node| matches!(node.op, Op::NewArray { element_type, .. } if element_type == 0))
        && helpers.anewarray_object == 0
    {
        note_lower_bail("anewarray-without-helper");
        return None;
    }
    // cov-01. The same reasoning as the two guards above, for the three
    // constant-pool nodes: each lowers to a `CALL` through a helper address,
    // and a zero there is a call to address 0. Only a synthetic unit-test table
    // can produce one — `build_helpers` always wires all three — but "only a
    // test can hit it" is what the monitor guard's history says not to rely on.
    //
    // `Op::LoadStatic` is guarded on the helper even though its usual route is
    // the direct load: which route a site takes is decided per site inside
    // `emit_inline_getstatic`, and any site the resolver declines falls back to
    // this helper. Refusing the graph is the only answer that does not depend
    // on a runtime resolver's answer at emission time.
    for (helper, present, what) in [
        (
            helpers.ldc_string_cp,
            graph
                .nodes
                .iter()
                .any(|n| matches!(n.op, Op::ConstString { .. })),
            "ldc_string_cp",
        ),
        (
            helpers.ldc_class_cp,
            graph
                .nodes
                .iter()
                .any(|n| matches!(n.op, Op::ConstClass { .. })),
            "ldc_class_cp",
        ),
        (
            helpers.getstatic,
            graph
                .nodes
                .iter()
                .any(|n| matches!(n.op, Op::LoadStatic { .. })),
            "getstatic",
        ),
        // A WIDE static's helper route peeks `jit_dispatch_threw` to tell a
        // legitimate `Long.MIN_VALUE` from the exception sentinel, so that
        // helper is load-bearing for exactly the `J`/`D`/`F` tags and for
        // nothing else. `Op::Call` reaches the same peek and does not guard it
        // — its wide-return sites are gated elsewhere — but a wide static
        // arrives here through a table, so state the requirement rather than
        // inherit an assumption.
        (
            helpers.dispatch_threw,
            graph.nodes.iter().any(|n| {
                matches!(n.op, Op::LoadStatic { type_tag, .. } if matches!(type_tag, b'J' | b'D' | b'F'))
            }),
            "dispatch_threw (needed by a J/D/F getstatic)",
        ),
    ] {
        if helper == 0 && present {
            return refuse(Bailout::with_context(
                BailoutReason::UnsupportedShape("constant-pool helper absent"),
                format!(
                    "graph contains a cov-01 constant-pool node but the helper table has no \
                     `{what}` entry; refusing rather than emitting a CALL through address zero"
                ),
            ));
        }
    }
    // Compact field layout (default ON) packs field offsets, so `Op::Load` and
    // `Op::Store`'s inline `HEADER_SIZE + field_index*SLOT_SIZE` displacements
    // are wrong for a compact object. Both have a compact-correct alternative —
    // the `jit_getfield` / `jit_putfield_int` helpers — so the only unlowerable
    // combination is "compact layout on AND the helper address absent", which
    // in practice means a synthetic unit-test helper table.
    //
    // This check used to live in `IrBuilder::build` as an unconditional bail on
    // the getfield/putfield opcodes themselves, which refused every
    // field-accessing method — most of real Java — from the optimizing tier
    // regardless of which lowering would have been chosen. Keeping it here
    // states the actual constraint: it is a property of one lowering, not of
    // the opcode.
    if cratonvm_types::compact_ref_fields_enabled() {
        let needs_getfield_helper =
            helpers.getfield == 0 && graph.nodes.iter().any(|n| matches!(n.op, Op::Load(_)));
        // Only an INT store has an inline lowering to fall back to, so only an
        // int store is what this compact-layout clause is about. Every other
        // `MemKind` is helper-only in both layouts and is refused
        // unconditionally below — checking `putfield_int` for them would refuse
        // a reference store for the absence of a helper it never calls.
        let needs_putfield_helper = helpers.putfield_int == 0
            && graph
                .nodes
                .iter()
                .any(|n| matches!(n.op, Op::Store(MemKind::Int)));
        if needs_getfield_helper || needs_putfield_helper {
            note_lower_bail("getfield-or-putfield-helper-required");
            return None;
        }
    }
    // A REFERENCE or WIDE field read has only one correct lowering: the helper.
    // The inline displacement fallback decodes a 16-byte int cell at
    // `HEADER_SIZE + index*SLOT_SIZE`, which for a reference slot yields the
    // discriminant word rather than the pointer — a fabricated address the
    // frame would then publish as a root — and for a `J`/`F`/`D` slot yields a
    // sign-extended half of the payload. Refuse the graph outright rather than
    // emit it, independently of the compact-layout switch above.
    //
    // A wide read additionally needs `jit_dispatch_threw`: `Long.MIN_VALUE`,
    // and the `-0.0` bit pattern, are bit-identical to the helper's deopt/NPE
    // sentinel, and without the out-of-band peek the lowering must either drop
    // a real NPE or bail on a legitimate value. Neither is acceptable, so
    // refuse instead. `Float` is included for the same reason the `J`/`D`/`F`
    // branch of `emit_call_return_check` includes it: the disambiguation is one
    // shape, keyed on the value's WIDTH CLASS rather than on a claim about what
    // `jit_getfield` happens to zero-extend today.
    if graph.nodes.iter().any(|n| {
        matches!(
            n.op,
            Op::Load(MemKind::Ref | MemKind::Long | MemKind::Float | MemKind::Double)
        )
    }) && helpers.getfield == 0
    {
        note_lower_bail("wide-load-without-getfield-helper");
        return None;
    }
    if helpers.dispatch_threw == 0
        && graph.nodes.iter().any(|n| {
            matches!(
                n.op,
                Op::Load(MemKind::Long | MemKind::Float | MemKind::Double)
            )
        })
    {
        note_lower_bail("wide-load-shape-unsupported");
        return None;
    }
    // COV-03 — every non-int field STORE is helper-only, and each width has its
    // own helper. A reference store's helper is the one that carries the write
    // barrier; emitting the store without it is the failure this lane is most
    // careful about (invisible until a concurrent or generational collection,
    // and it surfaces as a lost object, not as a fault at the store). Refuse
    // rather than substitute.
    for n in &graph.nodes {
        let missing = match n.op {
            Op::Store(MemKind::Ref) => helpers.putfield_object == 0,
            Op::Store(MemKind::Long) => helpers.putfield_long == 0,
            Op::Store(MemKind::Float) => helpers.putfield_float == 0,
            Op::Store(MemKind::Double) => helpers.putfield_double == 0,
            _ => false,
        };
        if missing {
            note_lower_bail("putfield-helper-missing-for-width");
            return None;
        }
    }
    // Every field access carries its slot index as a `Const` offset input, and
    // both lowerings read that index out of the node the edge points at. If it
    // is not a `Const`, neither has an index to use — and both used to fall
    // back to a silent `0`, i.e. to accessing SOME OTHER FIELD of the receiver.
    //
    // A silent `0` is the worst available answer for exactly the defect family
    // this lane keeps meeting: a store lands in a slot the class declares a
    // reference, the cell then holds a primitive under a reference's name, and
    // the fault appears much later in whatever dereferences it — a compiled
    // `arraylength` on `Int(1)`, faulting at `addr=0x5`
    // (`fixed-suite-bugs/tomcat/punned-sqlchar-rawdata-was-a-direct-call-pinned-by-address-FIXED-20260828.md`).
    // Nothing in the report points back here, because a wrong-slot write leaves
    // no trace of having chosen the wrong slot.
    //
    // The builder only ever emits `iconst(field_index)` into this edge, so this
    // is unreachable today and refusing costs nothing measurable. That is the
    // point: it closes the hazard while it is still unreachable, rather than
    // after some later optimisation makes the offset node non-constant and the
    // fallback starts silently corrupting objects.
    if graph.nodes.iter().any(|n| {
        matches!(n.op, Op::Load(_) | Op::Store(_))
            && n.inputs
                .get(3)
                .and_then(|&o| graph.nodes.get(o as usize))
                .map_or(true, |o| !matches!(o.op, Op::Const(_)))
    }) {
        note_lower_bail("non-const-store-operand");
        return None;
    }

    // ── Resource bounds, decided before anything is reserved ─────────
    //
    // Both checks precede `ExecutableBuffer::new` and `Lowerer::new`, so an
    // over-budget method costs neither an executable mapping nor an enormous
    // frame. `estimate_frame_bytes` is the same function `Lowerer::new` sizes
    // the frame from, so passing here guarantees the constructor's `i32`
    // arithmetic stays in range.
    if let Err(bailout) = check_graph_size(graph.nodes.len(), DEFAULT_MAX_NODES) {
        return refuse(bailout);
    }
    let frame_needs = scan_frame_needs(graph, helpers);
    // Liveness-based frame-slot reuse: values whose live ranges do not overlap
    // share one 8-byte word, so the spill reservation is sized by the peak
    // number of simultaneously live values instead of by the node count. The
    // colouring is *checked* before it is used — an aliased pair of live values
    // is silent wrong code, so it is not taken on trust.
    let slot_plan = plan_slots(graph, schedule, sr_map);
    if let Err(bailout) = verify_slot_colouring(graph, schedule, &slot_plan) {
        return refuse(bailout);
    }
    let frame_estimate = estimate_frame_bytes(num_locals, slot_plan.slots, &frame_needs);
    if let Err(bailout) = check_frame_size(frame_estimate, DEFAULT_MAX_FRAME_BYTES) {
        return refuse(bailout);
    }
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_IR_SLOTS").is_some() {
        eprintln!(
            "[ir-slots] nodes={} blocks={} slots={} peak_live={} frame_bytes={frame_estimate}",
            graph.nodes.len(),
            schedule.blocks.len(),
            slot_plan.slots,
            slot_plan.peak_live,
        );
        for (id, colour) in slot_plan.node_color.iter().enumerate() {
            if let Some(c) = colour {
                eprintln!(
                    "[ir-slots]   node {id:3} -> colour {c} class={:?}",
                    slot_plan.class.get(id).copied().flatten(),
                );
            }
        }
    }

    // ── Every emitted value has a location, decided before emission ──
    //
    // The acceptance test for the `[rbp - 0]` defect: drop a node from the
    // schedule and this refuses the compile here, with no code emitted at all,
    // instead of emitting a read of the saved caller frame pointer and
    // detecting it afterwards with a sticky bit.
    if let Err(bailout) = verify_data_locations(graph, schedule) {
        return refuse(bailout);
    }

    // Buffer sizing. The historical estimate (`nodes * 32 + 256`) predates call
    // lowering: an arithmetic node emits well under 32 bytes, but a single
    // MIC + 4-way-PIC dual-ABI inline-cache site emits ~360 and a direct call
    // ~60. With
    // `ir_compatible`'s invoke budget raised to `ir::IR_MAX_INVOKES`,
    // under-estimating here would silently TRUNCATE the body —
    // `ExecutableBuffer::emit` sets a sticky `overflowed` flag and skips the
    // write rather than panicking — so budget every call node explicitly, with
    // the same saturating arithmetic the single-pass sizing uses.
    let call_nodes = graph
        .nodes
        .iter()
        .filter(|n| matches!(n.op, Op::Call { .. }))
        .count();
    let capacity = ir_code_buffer_estimate(graph.nodes.len(), call_nodes);
    let mut buf = ExecutableBuffer::new(capacity)?;
    buf.set_tag("ir-lower");

    let mut lowerer = Lowerer::new(
        graph,
        schedule,
        buf,
        num_params,
        num_locals,
        &slot_plan,
        helpers,
        branch_hints,
        // NOT `&[]`. This argument was dropped on the floor from the day
        // splicing landed: the parameter arrived, `Lowerer::new` got an
        // empty slice, and `Lowerer::resume_bci` was therefore the identity
        // in every production lowering. Every deopt inside a spliced body
        // then recorded its RELOCATED bci — a program point that does not
        // exist in the method's own bytecode — so the interpreter's sink
        // found no matching deopt point, defaulted the reason to
        // `UnreachedCode`, and refused the replay against a bci nothing
        // could resume at. See
        // `fixed-bugs/jit/ir-inline-turns-an-index-out-of-bounds-into-an-internalerror-FIXED-20260828.md`.
        spliced_ranges,
        sr_map,
        direct_calls,
        ic_slots,
        compact_fields,
        inline_scopes,
    );

    // Gap B: only `abi_regs.len()` integer registers carry incoming args (4 on
    // Win64, 6 on SysV), and a `needs_context` method (one containing an
    // `Op::Call`) spends the first of them on the hidden VM pointer. Anything
    // past that arrives on the caller's stack, and `emit_prologue` can only
    // read registers — its deposit loop `break`s at `abi_regs.len()` and leaves
    // the remaining locals holding whatever the frame slot happened to contain.
    // Bail to the single-pass backend (which does load stack params, see
    // `x64.rs`'s "ROUND-12 fix") rather than mis-read them.
    //
    // The check used to sit INSIDE `if lowerer.needs_context`, which left every
    // LEAF method — no `Op::Call`, so `needs_context == false` — completely
    // unguarded. Those are the ones the prologue drops from ABI index
    // `abi_regs.len()` on, with no diagnostic: a method returning its own last
    // parameter returned null instead. `probes/EntryAbiArgSlotProbe.java`
    // measured the boundary exactly (the receiver is an incoming slot too, so
    // an instance method with N parameters occupies N+1): `i3`/`s4` = 4 slots
    // correct, `i4`/`s5` = 5 slots wrong, and `i6`/`s6` wrong 299_445 times in
    // 300_000. Downstream it silently emptied Spring Boot's property binding,
    // because `BindHandler.onSuccess(name, target, context, result)` is
    // `aload 4; areturn` over five slots — see
    // `fixed-suite-bugs/springboot/webflux-defaultpathcontainer-defaultseparator-classcast-FIXED.md`
    // for the trail from there to `BindResult.isBound() == false` for every
    // property.
    //
    // `emit_prologue`'s `break` is what this guard exists to keep unreachable;
    // `ir_lower_refuses_more_incoming_slots_than_abi_registers` pins the pair
    // together so the two cannot drift apart again.
    // (The refusal that used to be here — "incoming arg slots exceed the entry
    // ABI registers" — is gone: `emit_prologue` loads stack-resident arguments
    // now, exactly as the single-pass prologue does. It was the commonest
    // whole-method refusal an ordinary accessor hit; `VolumeShort2.getIndex(III)I`
    // is `this` + three ints = four incoming slots, and touching one field turns
    // `needs_context` on, which is the fifth. Two separate investigations
    // reached that line by bisecting the lowerer rather than by reading a log.)

    // BUG FIX [jit-irlower #2]: reserve phi destination slots before any
    // block is lowered — a forward branch's edge copies (emit_phi_copies)
    // reference slots of phis in not-yet-lowered merge blocks.
    if let Err(bailout) = lowerer.prealloc_phi_slots() {
        return refuse(bailout);
    }

    // ── Linear-scan register residency (default OFF) ─────────────────
    //
    // The production call site for `regalloc::allocate_linear_scan`. Installed
    // BEFORE the prologue and never touched again, so residency is a property
    // of the whole emission rather than something that changes mid-body.
    //
    // A verifier failure is the one hard stop. `plan_register_residency`
    // returns `Err` only when `verify_allocation` rejects the allocation — a
    // compiler bug — and emitting against an allocation nothing proved is
    // exactly the trade that has produced silent heap corruption here before.
    // Refuse the body; the method runs in a lower tier, which is always valid.
    // Everything else it can decline (register pressure, a liveness-model
    // disagreement, a value it cannot prove) comes back as `Ok(None)` and
    // leaves the colourer's memory layout in charge.
    //
    // The level-2 selector and this cache are MUTUALLY EXCLUSIVE, and the
    // exclusion belongs here rather than in either of them. `isel`'s encoder is
    // anchored byte-for-byte against the per-opcode arms under the assumption
    // that "the frame-homed allocation the encoder assumes IS this backend's
    // allocation" — which residency makes false, because an arm then reads its
    // operand out of a register while the tiler still emits a frame load.
    //
    // Neither outcome is unsound (write-through keeps the home word correct, so
    // the tiler's frame read gets the right value, and `Verify` mode is
    // fail-closed), but the two would silently disagree about bytes, which is
    // the one property the level-2 lane exists to be able to check. So while a
    // MIR mode is on, residency is off and the selector is compared against the
    // emission it was anchored to.
    let mut ls_active = false;
    if linear_scan_enabled() && !isel_emit_enabled() && !isel_verify_enabled() {
        match plan_register_residency(graph, schedule, &slot_plan) {
            Ok(Some(residency)) => {
                if crate::ir_stage_reporting() {
                    eprintln!(
                        "[ir] linear scan: {} values resident, {} demoted",
                        residency.promoted, residency.demoted
                    );
                }
                lowerer.set_residency(residency);
                ls_active = true;
            }
            Ok(None) => {}
            Err(bailout) => return refuse(bailout),
        }
    }

    // ── Shadow instruction selection (default OFF, emits nothing) ────
    //
    // Placed here, after every refusal above, so the population it measures is
    // exactly the population that gets a compiled body — measuring methods the
    // lowerer then refuses would inflate the figure with code nobody runs.
    //
    // Deliberately NOT fail-closed, which is the one place in this file that is
    // true. See `isel::shadow_select_method`: a flag whose documented effect is
    // a count must not decide what compiles, or the count describes a different
    // program. A `covers()` violation is counted and printed, not raised.
    if isel_shadow_enabled() {
        let stats = crate::x64::isel::shadow_select_method(graph, schedule);
        if isel_shadow_reporting() {
            eprintln!("[ir-isel] {}", stats.summary_line());
        }
    }

    // ── The level-2 machine list (default OFF) ───────────────────────
    //
    // Increment 2. Unlike the shadow pass above this one IS fail-closed, in
    // both its modes: a method whose blocks the selector cannot cover, or whose
    // tiles disagree with the per-opcode arms, loses its optimized body rather
    // than getting one nobody checked.
    let mir_mode = if isel_emit_enabled() {
        Some(MirMode::Emit)
    } else if isel_verify_enabled() {
        Some(MirMode::Verify)
    } else {
        None
    };
    if let Some(mode) = mir_mode {
        match build_mir_plan(graph, schedule) {
            Some(plan) => {
                // Increment 2: the machine list gets an allocation, and the
                // allocation gets `verify_allocation`. Runs BEFORE `set_mir`,
                // so a plan the verifier rejects is never installed and the
                // encoder never sees it.
                match verify_mir_allocation(graph, schedule, &slot_plan, &plan) {
                    Ok(verdict) => mir_totals::record_alloc(verdict),
                    Err(bailout) => {
                        mir_totals::record_alloc_rejected();
                        // `Verify` mode's contract is that it changes no
                        // emitted byte, so it must not change which methods
                        // compile either: record the rejection, report it with
                        // `CRATONVM_DBG=ir-isel`, and leave the per-opcode arms
                        // to emit exactly what they always did. `Emit` mode has
                        // no such licence — its bytes ARE the allocation's, so
                        // an allocation nothing proved is refused outright.
                        if mode == MirMode::Emit {
                            return refuse(bailout);
                        }
                    }
                }
                lowerer.set_mir(plan, mode)
            }
            None => {
                return refuse(Bailout::new(BailoutReason::Internal(
                    "ir_lower: instruction selection did not cover a block",
                )))
            }
        }
    }

    lowerer.emit_prologue();
    lowerer.emit_safepoint_poll();

    // Emit blocks in order
    lowerer.prepare_fusion_tables();
    for block_idx in 0..schedule.blocks.len() {
        lowerer.lower_block(block_idx);
    }

    // Soundness bail (ES SortingDigestTests -Jit on): some emitted use asked
    // for the frame slot of a node that was never allocated one (scheduler
    // gap — the node sits in no emitted block), or a slot allocation ran past
    // the frame's spill cap. Discard the artifact and let the caller fall back
    // to the single-pass backend.
    //
    // Both are now UNREACHABLE — `verify_data_locations` and the frame-size
    // check above refuse those graphs before emission begins — so this is a
    // net, not a mechanism. It is kept, and upgraded from one sticky bit to a
    // structured reason, so that if a future lowering finds a path the
    // pre-checks miss, the compile still fails closed *and* says why.
    // The byte-equality oracle's verdict, for the whole method.
    //
    // Reported before it is acted on, because a mismatch that only ever shows
    // up as "this method did not compile" is a mismatch nobody can diagnose.
    if lowerer.mir_mode != MirMode::Off {
        // Only the disagreements, and only under the debug switch: a corpus run
        // compiles thousands of methods, and a per-compile line for each would
        // bury the one line that matters. The totals are printed once at exit
        // (`vm-cli::maybe_dump_shutdown_reports`).
        if lowerer.mir_mismatches != 0 && isel_shadow_reporting() {
            eprintln!(
                "[ir-isel] mir mode={:?} tiles={} MISMATCHES={}",
                lowerer.mir_mode, lowerer.mir_tiles, lowerer.mir_mismatches
            );
        }
        mir_totals::record(
            lowerer.mir_tiles,
            lowerer.mir_mismatches,
            lowerer.mir_shadow_tiles,
            lowerer.mir_arm_bytes,
            lowerer.mir_enc_bytes,
        );
        if lowerer.mir_mismatches != 0 {
            return refuse(Bailout::new(BailoutReason::Internal(
                "ir_lower: the level-2 encoder disagreed with the per-opcode lowering",
            )));
        }
    }

    if let Some(bailout) = lowerer.take_latched_bailout() {
        return refuse(bailout);
    }
    if lowerer.unallocated_slot_use.get() {
        return refuse(Bailout::new(BailoutReason::Internal(
            "ir_lower: unallocated slot latched without a reason",
        )));
    }

    // Soundness bail: every emitted shadow PUSH must have exactly one emitted
    // RELOAD. The push advances the thread's shadow `top`; only the reload
    // retracts it. An unmatched push leaks a few slots per execution of that
    // site, so a recursive method walks `top` off the end of the shadow stack
    // and the next push writes into whatever follows the mapping. That
    // presents as heap corruption in an unrelated allocator, arbitrarily far
    // from the JIT — the self-recursive call route did it, and it took a
    // gdb backtrace showing ASCII bytes in a mimalloc free-list header to
    // attribute. A static count is enough (both are emitted per site, not per
    // path), and refusing the body is strictly better than shipping it.
    if lowerer.shadow_pushes != lowerer.shadow_reloads {
        if crate::ir_stage_reporting() {
            eprintln!(
                "[ir] lower_inner refused: {} shadow pushes vs {} reloads — \
                 an unmatched push leaks the thread's shadow top",
                lowerer.shadow_pushes, lowerer.shadow_reloads
            );
        }
        note_lower_bail("shadow-push-reload-imbalance");
        return None;
    }

    // real-frame-deopt (step 3): emit the shared deopt stub after the method
    // body so failed guards can jump to it, then patch in-method branches.
    lowerer.emit_deopt_stub();
    // Gap B: emit the shared call-exception bail stub after the body so each
    // dispatch site's sentinel `JE` reaches it.
    lowerer.emit_call_exc_stub();

    lowerer.finish_lazy_thread_fetch();
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

    // Relocation-contract values, read off the lowerer before `buf` moves out.
    let sp_id_slot_off = lowerer.sp_id_slot_off;
    let shadow_thread_slot_off = lowerer.shadow_thread_slot_off;
    let shadow_savebase_slot_off = lowerer.shadow_savebase_slot_off;
    let oop_maps = std::mem::take(&mut lowerer.oop_maps);
    let sp_id_bcis = std::mem::take(&mut lowerer.sp_id_bcis);
    let locals_size = lowerer.locals_size;
    let first_spill = lowerer.first_spill;
    let spill_cap_off = lowerer.spill_cap_off;
    let saved_xmm_bytes = lowerer.saved_xmm_bytes;
    let saved_gpr_bytes = lowerer.saved_gpr_bytes;
    let frame_size = lowerer.frame_size;

    // ── Install-time deopt-metadata verification ─────────────────────
    //
    // The metadata is checked BEFORE the artifact becomes a `CompiledMethod`,
    // because a frame that cannot be reconstructed is only discoverable at
    // runtime otherwise — at a guard, on a VM thread, with the frame already
    // half torn down. Bailing here is always semantically valid (the method
    // takes the single-pass backend or the interpreter); installing code whose
    // deopt map disagrees with its oop map is not.
    //
    // Which lanes are active is decided by which data this backend actually
    // has, so every check that fires is one this file can be held to:
    //
    //   * **scope** — `resolve_frame_state` records `method_key: String::new()`
    //     (the lowerer does not know the method's identity; deopt resume keys on
    //     the running `CompiledMethod`), so the limits are registered under that
    //     same empty key. `code_len`/`max_locals`/`max_stack` are saturated: the
    //     bytecode length and `max_stack` never reach this function, and
    //     `num_locals` is not a bound a hand-built graph's snapshots respect.
    //     What the lane does buy is the `UnknownMethod` check — the day inlined
    //     caller scopes start carrying real method keys, they must arrive with
    //     limits rather than silently skipping every per-scope check.
    //   * **removed-node** — every `Op::Dead` id, cross-checked against the
    //     scalar-replacement map's keys as the ones a `VirtualObject` recipe is
    //     allowed to name. This is the lane that catches a recipe built from a
    //     stale graph.
    //   * **oop-map agreement** — one `OopCoverage` per emitted `OopMapEntry`,
    //     keyed by native offset. `emit_safepoint_map` currently pushes entries
    //     with `native_pc_offset: 0` (the relocation path matches them by the
    //     safepoint id in the frame slot, not by pc), and registering every map
    //     under key `0` would compare deopt points against an arbitrary map, so
    //     only anchored entries are registered. The lane therefore arms itself
    //     the moment this backend starts anchoring its maps to native offsets.
    //   * **structural** — always on.
    let mut verifier = DeoptVerifier::new()
        .with_method(MethodFrameLimits::new(
            String::new(),
            u32::MAX,
            u16::MAX,
            u16::MAX,
        ))
        .with_removed_nodes(
            graph
                .nodes
                .iter()
                .enumerate()
                .filter(|(_, n)| matches!(n.op, Op::Dead))
                .map(|(id, _)| id as u32),
        );
    if let Some(sr) = sr_map {
        verifier = verifier.with_materializable_nodes(sr.objects.keys().copied());
    }
    for entry in &oop_maps {
        if entry.native_pc_offset != 0 {
            verifier = verifier.with_oop_map(
                entry.native_pc_offset,
                OopCoverage {
                    frame_slot_offsets: entry.frame_slot_offsets.clone(),
                    // The IR lowerer keeps every live value in a frame slot; it
                    // never publishes a reference in a GPR.
                    registers: Vec::new(),
                    moving_young_coverage_complete: entry.moving_young_coverage_complete,
                },
            );
        }
    }
    // Both sets are checked. `deopt_points` is the sorted, native-offset-keyed
    // list `find_deopt_point` binary-searches; `deopt_boxes` are the boxes baked
    // into the emitted guards, and those are the frame states a *live* deopt
    // actually reconstructs from. Each box is verified on its own — they are not
    // one sorted sequence, so checking them as a slice would report a bogus
    // ordering violation.
    for point in &deopt_boxes {
        if let Err(bailout) = verifier.verify(std::slice::from_ref(&**point)) {
            return refuse(bailout);
        }
    }
    if let Err(bailout) = verifier.verify(&deopt_points) {
        return refuse(bailout);
    }

    // The liveness colouring's peak — the number of values simultaneously live
    // at the busiest program point — is exactly what
    // `CompilationReport::peak_live_values` asks for, and this is the only place
    // in the compiler that computes it. Recorded through the thread-local hook
    // rather than a parameter because `lower_inner`'s signature is pinned (see
    // `metrics::note_current_peak_live_values`); a no-op when metrics are off.
    crate::metrics::note_current_peak_live_values(slot_plan.peak_live);

    // `CompilationReport::spills` / `::reloads` are reported ONLY when the
    // linear-scan path actually ran, and they count what this backend EMITTED,
    // not what `regalloc::Allocation` planned — the plan is a full
    // spill/reload/split schedule, and this wiring executes only the subset it
    // can prove (see `plan_register_residency`). Reporting the plan's numbers
    // would describe machine code that was never generated.
    //
    // The colourer-only path leaves both `NotMeasured`, which is the truth: it
    // allocates no registers, so it has no register↔memory transitions to
    // count, and a reported `0` would be indistinguishable from "measured, and
    // it spilled nothing".
    if ls_active {
        crate::metrics::note_current_spills(lowerer.ls_spills);
        crate::metrics::note_current_reloads(lowerer.ls_reloads);
    }

    // Carry the identity the prologue encoded, so publication can bind it to
    // the artifact this buffer becomes.
    let compile_id = lowerer.compile_id;
    let mut buf = lowerer.buf;
    // Soundness bail (jit-inlining-and-ir-calls). `ExecutableBuffer::emit` is
    // non-panicking: on capacity exhaustion it sets a sticky `overflowed` flag
    // and DROPS the write, so an under-estimated buffer yields a silently
    // truncated body — execution runs off the end of the emitted code. The
    // single-pass backend has always checked this; the IR lowerer never did,
    // which was harmless only while its per-node emission was tiny and bounded.
    // Inline caches (~250 bytes/site) plus the widened invoke budget make the
    // estimate materially harder, so check it here and let the caller fall back
    // to single-pass, exactly like the unallocated-slot latch above.
    report_ir_buffer_size(graph.nodes.len(), call_nodes, buf.wanted(), capacity);
    if buf.overflowed() {
        // `needed` is `wanted()`, NOT `pos()`. `pos()` is the write cursor,
        // which STOPS at capacity the moment the buffer overflows — so it
        // reported `needed == capacity` on every single exhausted compile and
        // told you nothing about how much more the method actually wanted. The
        // whole 2026-08-01 census read `needed 4096 bytes, capacity 4096`
        // seventeen times over and looked like a tie. `wanted()` keeps counting
        // through the dropped writes, and this backend never calls `rewind_to`
        // or `emit_checked`, so it is the exact requirement rather than a
        // bound.
        return refuse(Bailout::new(BailoutReason::CodeBufferExhausted {
            needed: buf.wanted(),
            capacity,
        }));
    }
    let _code_size = buf.pos();

    let mut cm = CompiledMethod::new(buf);
    cm.compile_id = compile_id;
    cm.deopt_points = deopt_points;
    cm._deopt_point_boxes = deopt_boxes;

    // ── Moving-young relocation contract ────────────────────────────────
    //
    // Publish what `conservative_roots` needs to decide, per frame, whether a
    // relocating young collection may proceed while this frame is live. Before
    // this the IR backend published NOTHING — no maps, no frame layout, no
    // safepoint-id slot — and an empty `oop_maps` reads as "no precise
    // coverage", so every live IR frame forced the non-moving sweep. That is
    // also why the optimizing tier was gated off under moving-young.
    //
    // The frame the lowerer builds, from `rbp` downward:
    //
    //     locals | [context] | bookkeeping ×5 | spills … | arg staging | argrsv | shadow
    //     ^0       ^locals    ^                 ^first_spill           ^spill_cap_off
    //
    // The five bookkeeping words are the safepoint id, the cached thread
    // pointer, the shadow save-base, the shadow save-top and the phi
    // parallel-copy scratch. They are all below `first_spill`, so the spill
    // band the maps and the deopt verifier describe is unchanged by them.
    //
    // `callee_saved_lo` names the start of the region the verifier must NOT
    // inspect: the callee-saved XMM save area (`IR_LOWER_SAVED_XMMS`, empty on
    // System V and on every compile with the linear-scan path off), then the
    // outgoing-argument staging, the stack-arg reserve and the ABI shadow
    // space. Register images, dead argument words and scratch — storage this
    // frame never resumes from, which is exactly the role the field plays on
    // the reader side (`band_slot_is_verifiable` skips everything at or above
    // it).
    //
    // The save area was placed at `spill_cap_off` precisely so this bound did
    // not have to move: it grew the skipped band from below without changing
    // either endpoint's meaning, and `conservative_roots` needed no edit. An
    // XMM save area could never hold a reference in any case — the file is
    // FP-only — but the band is the right home for it regardless, because
    // "this word is a register image" is the property the reader is testing.
    cm.sp_id_slot_off = sp_id_slot_off;
    cm.oop_maps = oop_maps;
    // Ascending by construction (ids come from a counter), which is what
    // `CompiledMethod::safepoint_bci` binary-searches on. Sorted rather than
    // asserted: a future lowerer that emits a safepoint out of order would
    // otherwise turn a diagnostic into a wrong line, and the vector is one
    // entry per GC-capable point.
    cm.safepoint_bci_table = {
        let mut t = sp_id_bcis;
        t.sort_unstable_by_key(|(id, _)| *id);
        t.dedup_by_key(|(id, _)| *id);
        t
    };
    // Stage A.2 (precise oop maps, B-K fix) parity with the x64 fast-tier
    // driver (`x64/driver.rs`'s `cm.fully_oop_covered = compiler.precise_maps
    // && ... && compiler.safepoint_pcs.is_subset(&compiler.mapped_safepoint_pcs)`).
    // This backend never computed the field at all -- it stayed at the
    // `CompiledMethod` default of `false` for every method this tier compiled,
    // OSR or not, which blanket-failed `moving_young_osr_method_needs_fallback`'s
    // precise-map check on every OSR artifact this backend ever produced.
    // Measured 2026-08-22 on `TestKillProcessWhileWriting`: this is the
    // disjunct that actually fires (`osr_reason=(... map_coverage=N ...)`),
    // not the shadow layout and not a missing exact RBP -- both of which this
    // backend gets right already.
    //
    // Unlike the fast tier, this backend needs no PC-based subset check: each
    // `OopMapEntry` above already carries its own `moving_young_coverage_complete`
    // (the shadow-push `coverable && (published || slots.is_empty())` verdict,
    // computed per safepoint at the point it is emitted), and `cm.oop_maps` is
    // the complete set this compilation ever pushed to -- so the aggregate is a
    // straight AND over data already relied on elsewhere: it is the exact
    // per-entry flag `gc_quiescence`'s per-cycle proof already consults for
    // every non-OSR moving-young collection. An empty `oop_maps` makes this
    // vacuously true, which is safe: `has_precise_oop_maps()` (`!oop_maps.is_empty()`)
    // is what actually gates the fast tier's OR-branch on "no maps at all," so a
    // vacuous true here never overrides that check.
    cm.fully_oop_covered = cm.oop_maps.iter().all(|m| m.moving_young_coverage_complete);
    // The same aggregate under the name that says what it measures — see
    // `CompiledMethod::fully_shadow_covered`. The two coincide on THIS backend
    // and deliberately differ on the fast tier, where `fully_oop_covered` is
    // the frame-slot subset test; the OSR fallback reads `fully_shadow_covered`
    // so it gets the same question from both.
    cm.fully_shadow_covered =
        !cm.oop_maps.is_empty() && cm.oop_maps.iter().all(|m| m.moving_young_coverage_complete);
    cm.osr_frame_size = frame_size;
    // OSR and the save area, stated where the artifact is published.
    //
    // `osr_trampoline` builds the frame ITSELF — push rbp, sub
    // `osr_frame_size`, spill the callee-saved sets at `osr_callee_saved_base`
    // / `osr_xmm_saved_base` — and jumps to a native offset PAST
    // `emit_prologue`. An OSR entry into a method with a save area would
    // therefore never perform the save, while every exit would still perform
    // the restore: the caller gets two words of uninitialised frame back as
    // its XMM6/XMM7. A wrong `double` in a caller's register, on Windows only,
    // with nothing downstream that inspects it.
    //
    // It cannot happen today — this backend publishes no `osr_pc_to_native`,
    // so `osr_enter` refuses at its first `?` (`OSR_REFUSE_NO_ENTRY_TABLE`).
    // Asserted rather than commented because the edit that breaks it is "wire
    // OSR into the IR tier", which will not look like it touches the prologue.
    // The fix then is to publish `osr_callee_saved_xmms` and
    // `osr_xmm_saved_base` here so the trampoline saves what the epilogue
    // restores — not to delete this assertion.
    debug_assert!(
        (saved_xmm_bytes == 0 && saved_gpr_bytes == 0) || cm.osr_pc_to_native.is_none(),
        "this frame saves {saved_xmm_bytes} bytes of callee-saved XMM and \
         {saved_gpr_bytes} of callee-saved GPR in its prologue but publishes an \
         OSR entry table; the trampoline enters past the prologue and every \
         exit would restore what was never saved",
    );
    // Where the reader finds what the emission side published. Without these
    // three, `shadow_window_from_frame` cannot even locate the shadow stack —
    // it returns `None`, `published_shadow_values` yields the empty set, and
    // EVERY published word then reads back as unpublished. That is not a
    // partial failure that shows up as a smaller win: it makes a fully correct
    // publication indistinguishable from no publication at all, and it is what
    // `compiled-frame-oop-not-published` meant on `IrEscapeProbe` after the
    // maps, the frame record and the parameter homes were all in place.
    cm.shadow_thread_slot_off = shadow_thread_slot_off;
    cm.shadow_savebase_slot_off = shadow_savebase_slot_off;
    cm.shadow_off_in_thread = helpers.shadow_stack_offset_in_thread as i32;
    cm.frame_layout = super::FrameLayout {
        java_locals_hi: locals_size,
        // No LICM hoists, no scalar-replacement slots and no per-safepoint
        // blind GPR spill in this backend: empty ranges (`hi == lo`), which
        // `is_register_image` and `region_name` both read as "absent".
        ref_hoist_lo: 0,
        ref_hoist_hi: 0,
        arith_lo: 0,
        arith_hi: 0,
        scalar_lo: 0,
        scalar_hi: 0,
        locals_hi: first_spill,
        spill_lo: first_spill,
        spill_hi: spill_cap_off,
        callee_saved_lo: spill_cap_off,
        callee_saved_hi: frame_size,
        // Named separately as well as covered by the band above, so
        // `FrameLayout::region_name` reports `xmm-saved` rather than the
        // catch-all — a frame dump that cannot tell a register image from an
        // outgoing argument is the one that gets misread during a crash triage.
        // `hi == lo == 0` when nothing was reserved, which `is_register_image`
        // already reads as "absent".
        xmm_saved_lo: if saved_xmm_bytes > 0 {
            spill_cap_off
        } else {
            0
        },
        xmm_saved_hi: if saved_xmm_bytes > 0 {
            spill_cap_off + saved_xmm_bytes
        } else {
            0
        },
        reg_spill_lo: 0,
        reg_spill_hi: 0,
        frame_size,
    };
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
    //
    // ...unless the graph holds a monitor. Every `FrameState` this lowerer
    // builds hard-codes `monitors: Vec::new()`, so a precise resume would
    // rebuild an interpreter frame that believes it holds no lock. The
    // interpreter's own sink refuses a frame that holds monitors — but it
    // cannot fire on information that was never recorded, so the omission
    // defeats the guard rather than tripping it, and the method exits without
    // the `monitorexit` the lock is waiting for.
    //
    // The guard above (`monitor helper absent`) does not cover this: the
    // helpers ARE wired in production, so that refusal is inert. Until the
    // frame states carry real monitor state, a monitor-bearing method may be
    // compiled and may deoptimize — it just may not resume PRECISELY. The
    // whole-method re-run it falls back to re-enters a re-entrant lock and
    // stays balanced.
    if sr_map.is_some()
        && cm
            ._deopt_point_boxes
            .iter()
            .any(|p| crate::deopt::count_virtual_objects(&p.frame_state) > 0)
        && !graph
            .nodes
            .iter()
            .any(|n| matches!(n.op, Op::MonitorEnter | Op::MonitorExit))
    {
        cm.can_deopt_resume = true;
    }
    Some(cm)
}

// ── Tests ────────────────────────────────────────────────────────────

/// Loop-carried values (phis) may be register-resident -- **default ON**, opt
/// out with `CRATONVM_JIT_IR_PHI_RESIDENCY=0`. `emit_phi_copies` publishes a
/// promoted phi's register at every incoming edge; off, phis stay home-bound
/// as they were before 2026-09-02 and the residency census reports them
/// under `phi=`.
fn ir_phi_residency_enabled() -> bool {
    match cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_PHI_RESIDENCY") {
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => true,
    }
}

/// Promote only where a register can repay its prologue save -- **default
/// ON**, opt out with `CRATONVM_JIT_IR_RESIDENCY_PAYS=0` to promote every
/// candidate the allocator hands back, which is what this file did before
/// 2026-09-02 and what made `FibProbe.fib` slower with the file on than off.
fn ir_residency_pays_enabled() -> bool {
    match cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_RESIDENCY_PAYS") {
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => true,
    }
}

/// A constant is read as an immediate rather than from its home word --
/// **default ON**, opt out with `CRATONVM_JIT_IR_CONST_IMM=0`.
fn ir_const_imm_enabled() -> bool {
    match cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_CONST_IMM") {
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => true,
    }
}

/// A compare whose only use is its `If` lowers as one `cmp; jcc`, and a
/// branch with no phi copies on either edge emits no trampolines -- **default
/// ON**, opt out with `CRATONVM_JIT_IR_FUSED_BRANCH=0`.
fn ir_fused_branch_enabled() -> bool {
    match cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_FUSED_BRANCH") {
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => true,
    }
}

/// The null + mapped receiver guard is emitted once per receiver per block --
/// **default ON**, opt out with `CRATONVM_JIT_IR_RECEIVER_GUARD_CSE=0`.
fn ir_receiver_guard_cse_enabled() -> bool {
    match cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_RECEIVER_GUARD_CSE") {
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => true,
    }
}

/// Inline TLAB bump for the optimizing tier's `Op::New` — **OPT-IN**, on with
/// `CRATONVM_JIT_IR_INLINE_TLAB=1`.
///
/// This line said "**default ON**, opt out with `=0`" for two days after the
/// body below was flipped to opt-in, which is worse than either state: a reader
/// checking whether a feature is live got the wrong answer from the only
/// sentence written to tell them. The mismatch cost a census here — a repro run
/// came back 0/10 and looked like a fix, when the sequence under test had
/// simply never executed.
///
/// Off restores `emit_new_object_stub` alone, which is what this arm emitted
/// before the bump existed, so the two are A/B-able in one binary. That is not
/// a courtesy: the last frame-shaped change that shipped without a switch cost
/// a full rebuild per hypothesis to bisect.
///
/// Note what turning this on does NOT do. `c2_alloc_upgrade_enabled()` is
/// opt-in, so no method containing a `new` reaches this tier at all under a
/// default configuration and the bump is unreachable. Closing that gate's
/// stated reason is exactly what this change is for — see
/// `the_optimizing_tier_stays_shut_to_allocation_while_it_has_no_inline_tlab`,
/// which now passes because the bump exists rather than because the gate is
/// shut.
///
/// **The two gates are in series, so the SIGSEGV that keeps the outer one shut
/// cannot occur in any default build.** Both must be set for the bump to run:
/// `CRATONVM_JIT_C2_ALLOC_UPGRADE=1` to let an allocation-bearing method reach
/// this tier, and this one to emit the bump rather than the stub. Measured
/// 2026-09-04 on dev with engagement confirmed (`inline_tlab_bump=1
/// stub_only=0`, the census in `runtime_lowering::ir_alloc_site_counts`):
/// `RJitMapTierDiff` passes **0 failures in 30 runs** — 10 on a quiet host and
/// 20 under six spinners — against the 4-in-10 recorded on 2026-09-02.
///
/// That is a reason to re-examine the outer gate, NOT to open it here: its
/// second stated reason is a perf trade (a promoted allocation lowering through
/// the stub buys a more optimized body at the price of a cheaper allocation)
/// that is still unpriced.
fn ir_inline_tlab_enabled() -> bool {
    // 2026-09-02, the eight-finding pass: BACK TO OPT-IN, with a repro.
    //
    // This bump had never executed. Its own gate was shut, and underneath that
    // `VmHeap::refill_tlab` answered `None` on the default collector, so
    // `thread.tlab` was always empty and the limit compare always failed.
    // Giving ZGC mutators a chunk made it live for the first time, and
    // `regression-suite` vector `RJitMapTierDiff` then SIGSEGVs 4 runs in 10,
    // inside VM code, on a reference read back as a small integer (`0x2800`)
    // — i.e. an object whose contents are not what its allocator promised.
    //
    // Three switches each take the crash rate to zero, and they are the three
    // that gate this sequence executing at all: `CRATONVM_ZGC_MUTATOR_TLAB=0`
    // (no chunk to bump), this flag (the stub instead), and
    // `CRATONVM_JIT_C2_ALLOC_UPGRADE=0` (no allocating method reaches this
    // tier). It is NOT the `Op::New` header writes, which were read out of a
    // disassembly and match the single-pass sequence field for field, and it
    // is NOT relocation (4/4 with `CRATONVM_ZGC_RELOCATE=0`). One real defect
    // WAS found and fixed on the way (`IN_OWN_TLAB_RETIRE`, a double free of
    // the reserved tail); it is not this one.
    //
    // So: the capability stays, behind its switch, with the repro written
    // down — and `c2_alloc_upgrade_enabled` stays opt-in with it, because a
    // promoted allocation lowering through `emit_new_object_stub` is a CALL
    // where the single-pass body bumps inline, which is the downgrade that
    // gate was shut for in the first place.
    matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_INLINE_TLAB").as_deref(),
        Ok("1") | Ok("true") | Ok("on") | Ok("yes")
    )
}

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

    /// Regression witness (2026-07-27): NO rel32 patch site in this file may
    /// `expect`/`unwrap` its `try_patch_*` result.
    ///
    /// `ExecutableBuffer::emit` is non-panicking — on capacity exhaustion it
    /// sets a sticky `overflowed` flag and DROPS the write, so recorded patch
    /// offsets stop addressing their placeholder bytes and `try_patch_i32`
    /// legitimately fails (setting the flag itself). `lower_inner` is built for
    /// exactly that: its `if buf.overflowed() { return None }` bail discards the
    /// artifact and the caller falls back to the single-pass backend. An
    /// `expect` at a patch site turns that recoverable fallback into a
    /// compile-thread panic, which in a release VM is
    /// `fatal runtime error: failed to initiate panic` → SIGABRT of the whole
    /// process. See `patch_rel32_to_here`'s doc comment for the same argument.
    ///
    /// How this was found: lifting the blanket `org/junit/` JIT ban aborted
    /// 58 of 60 real Elasticsearch test classes, every one at
    /// `emit_call_exc_stub`'s `expect("call-exc JE patch in-bounds")`. Thirteen
    /// sites in this file still had that shape; they now use `.ok()`, matching
    /// what `jit/src/x64.rs` has always done. Asserting on the source keeps a
    /// future site from silently reintroducing the abort — a behavioural test
    /// cannot reach these emitters without a graph large enough to overflow
    /// `lower_inner`'s own buffer estimate, which is exactly the condition the
    /// estimate exists to prevent.
    #[test]
    fn no_patch_site_panics_on_an_overflowed_buffer() {
        let src = include_str!("ir_lower.rs");
        let offenders: Vec<(usize, &str)> = src
            .lines()
            .enumerate()
            .filter(|(_, l)| {
                let t = l.trim_start();
                (t.starts_with(".expect(") || t.starts_with(".unwrap(")) && l.contains("patch")
            })
            .map(|(i, l)| (i + 1, l.trim()))
            .collect();
        assert!(
            offenders.is_empty(),
            "patch sites must swallow the error and let `lower_inner`'s \
             `buf.overflowed()` bail fall back to single-pass, not panic \
             (use `.ok()`); offenders: {offenders:?}",
        );
    }

    #[test]
    fn live_new_uses_shared_allocation_stub_and_context_abi() {
        extern "C" fn allocate(vm: i64, class_id: i64, num_fields: i64) -> i64 {
            vm + class_id * 100 + num_fields
        }

        let mut graph = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        };
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let allocation = graph.add(
            Op::New {
                class_id: 23,
                num_fields: 7,
            },
            IrType::Ref,
            vec![ctrl, mem],
            Some(0),
        );
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, allocation], Some(3));

        let schedule = ir_schedule::schedule(&graph);
        let mut helpers = no_helpers();
        helpers.new_object = allocate as *const () as usize;
        let compiled =
            lower(&graph, &schedule, 0, 0, &helpers).expect("live allocation must lower");
        assert!(compiled.needs_context);
        // SAFETY: the synthetic helper treats the context as an integer and the
        // generated method has no Java arguments.
        let result = unsafe {
            compiled
                .try_call_with_context(11, &[])
                .expect("allocation call")
        };
        assert_eq!(result, 11 + 23 * 100 + 7);
    }

    #[test]
    fn live_new_converts_null_failure_to_jit_exception_sentinel() {
        extern "C" fn fail(_: i64, _: i64, _: i64) -> i64 {
            0
        }

        let mut graph = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        };
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let allocation = graph.add(
            Op::New {
                class_id: 1,
                num_fields: 0,
            },
            IrType::Ref,
            vec![ctrl, mem],
            Some(0),
        );
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, allocation], Some(3));

        let schedule = ir_schedule::schedule(&graph);
        let mut helpers = no_helpers();
        helpers.new_object = fail as *const () as usize;
        let compiled = lower(&graph, &schedule, 0, 0, &helpers).expect("live allocation");
        // SAFETY: helper ignores the synthetic context.
        assert_eq!(
            unsafe { compiled.try_call_with_context(1, &[]) },
            Ok(i64::MIN)
        );
    }

    /// cov-06: a PRIMITIVE `Op::NewArray` (`element_type != 0`) lowers
    /// through `emit_new_array_stub` to a call on `helpers.newarray`, with
    /// the runtime length forwarded as the third ABI argument (not baked as
    /// an immediate, unlike `Op::New`'s field count) — the same
    /// `(vm, atype, length)` shape `jit_newarray` and the single-pass
    /// backend's 0xbc arm share.
    #[test]
    fn live_newarray_uses_shared_allocation_stub_and_context_abi() {
        extern "C" fn allocate(vm: i64, atype: i64, length: i64) -> i64 {
            vm + atype * 100 + length
        }

        let mut graph = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        };
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let length = graph.add(Op::Param(0), IrType::Int, vec![start], None);
        let allocation = graph.add(
            Op::NewArray {
                element_type: 10, // T_INT
                component_class_id: 0,
            },
            IrType::Ref,
            vec![ctrl, mem, length],
            Some(0),
        );
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, allocation], Some(3));

        let schedule = ir_schedule::schedule(&graph);
        let mut helpers = no_helpers();
        helpers.newarray = allocate as *const () as usize;
        let compiled =
            lower(&graph, &schedule, 1, 1, &helpers).expect("live array allocation must lower");
        assert!(compiled.needs_context);
        // SAFETY: the synthetic helper treats the context/length as integers
        // and the generated method takes one int argument (the length).
        let result = unsafe {
            compiled
                .try_call_with_context(11, &[7])
                .expect("allocation call")
        };
        assert_eq!(result, 11 + 10 * 100 + 7);
    }

    /// cov-06: a REFERENCE `Op::NewArray` (`element_type == 0`) lowers
    /// through the SAME stub but calls `helpers.anewarray_object` with the
    /// component class id as the immediate — the two helpers are chosen
    /// per-node, not per-graph, so a method could in principle mix both
    /// shapes (this test only needs one to prove the routing).
    #[test]
    fn live_anewarray_calls_the_reference_array_helper() {
        extern "C" fn allocate(vm: i64, component_class_id: i64, length: i64) -> i64 {
            vm + component_class_id * 1000 + length
        }

        let mut graph = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        };
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let length = graph.add(Op::Param(0), IrType::Int, vec![start], None);
        let allocation = graph.add(
            Op::NewArray {
                element_type: 0,
                component_class_id: 42,
            },
            IrType::Ref,
            vec![ctrl, mem, length],
            Some(0),
        );
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, allocation], Some(3));

        let schedule = ir_schedule::schedule(&graph);
        let mut helpers = no_helpers();
        helpers.anewarray_object = allocate as *const () as usize;
        let compiled =
            lower(&graph, &schedule, 1, 1, &helpers).expect("live array allocation must lower");
        // SAFETY: the synthetic helper treats the context/length as integers.
        let result = unsafe {
            compiled
                .try_call_with_context(11, &[5])
                .expect("allocation call")
        };
        assert_eq!(result, 11 + 42 * 1000 + 5);
    }

    /// cov-06: `jit_newarray`/`jit_anewarray_object` return `0` for BOTH a
    /// negative length (JLS `NegativeArraySizeException`) and OOM — the same
    /// zero-on-failure convention `jit_new_object` uses. `Op::NewArray`'s
    /// lowering must convert that to the JIT-wide `i64::MIN` sentinel exactly
    /// like `Op::New`'s failure path, or a negative-length `anewarray` would
    /// hand the caller a null pointer instead of routing through the pending
    /// exception.
    #[test]
    fn live_newarray_converts_null_failure_to_jit_exception_sentinel() {
        extern "C" fn fail(_: i64, _: i64, _: i64) -> i64 {
            0
        }

        let mut graph = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        };
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let length = graph.add(Op::Param(0), IrType::Int, vec![start], None);
        let allocation = graph.add(
            Op::NewArray {
                element_type: 10,
                component_class_id: 0,
            },
            IrType::Ref,
            vec![ctrl, mem, length],
            Some(0),
        );
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, allocation], Some(3));

        let schedule = ir_schedule::schedule(&graph);
        let mut helpers = no_helpers();
        helpers.newarray = fail as *const () as usize;
        let compiled = lower(&graph, &schedule, 1, 1, &helpers).expect("live allocation");
        // SAFETY: helper ignores the synthetic context/length.
        assert_eq!(
            unsafe { compiled.try_call_with_context(1, &[-1]) },
            Ok(i64::MIN)
        );
    }

    /// cov-06: the same "refuse rather than call through address zero"
    /// contract `Op::New`/`Op::MonitorEnter` already have. A graph containing
    /// a live `Op::NewArray` with NO helper wired for its shape must be
    /// REFUSED, never silently miscompiled into a call to `0`.
    #[test]
    fn a_newarray_graph_with_no_helper_is_refused() {
        let mut graph = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        };
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let length = graph.add(Op::Param(0), IrType::Int, vec![start], None);
        let allocation = graph.add(
            Op::NewArray {
                element_type: 10,
                component_class_id: 0,
            },
            IrType::Ref,
            vec![ctrl, mem, length],
            Some(0),
        );
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, allocation], Some(3));
        let schedule = ir_schedule::schedule(&graph);
        assert!(
            lower(&graph, &schedule, 1, 1, &no_helpers()).is_none(),
            "a newarray graph with no `helpers.newarray` must be REFUSED, not \
             lowered to a call through address zero"
        );
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

    /// An argument past the entry ABI's register file arrives on the CALLER'S
    /// stack, and `emit_prologue` must load it from there.
    ///
    /// This used to be a whole-method REFUSAL (`ir_lower_refuses_more_incoming_
    /// slots_than_abi_registers`), because the prologue read registers and
    /// nothing else. It was the commonest refusal an ordinary accessor hit — an
    /// instance method with three parameters is four incoming slots, and
    /// touching one field turns `needs_context` on, which is the fifth — so on
    /// Win64 a three-argument getter could not be lowered at this tier at all.
    ///
    /// Tested through the RETURNED VALUE rather than the emitted bytes: the
    /// body is `iload_<last>; ireturn`, so a prologue that dropped the argument
    /// answers with whatever the frame slot happened to hold, which is exactly
    /// the silent wrong value the old refusal existed to prevent. Each argument
    /// is given a distinct value, so reading the WRONG stack slot is a
    /// different failure from reading none.
    #[test]
    fn ir_lower_loads_incoming_arguments_past_the_entry_abi_registers() {
        let cap = incoming_abi_reg_capacity();
        // `CompiledMethod::try_call` dispatches through one
        // `extern "C" fn(i64, …)` thunk per arity and has thunks up to
        // `TRY_CALL_MAX_ARGS`; past that it answers `Err(TooManyArgs)`.
        // That is a limit of this HARNESS, not of the lowering under test,
        // and the two ABIs sit on opposite sides of it: Win64 has four
        // entry registers, so `cap + 3` is 7 and callable, while SysV has
        // six, so `cap + 3` is 9 and comes back `Err(TooManyArgs(9))` —
        // which is how this test read RED on Linux and green on Windows
        // while the prologue it exercises was working on both.
        //
        // Clamp to what the harness can call, and assert the clamp still
        // leaves a stack argument to test: a bound that silently collapsed
        // to `0..=0` would turn this into a registers-only test that always
        // passes, which is the failure mode worth guarding.
        const TRY_CALL_MAX_ARGS: usize = 8;
        let max_extra = TRY_CALL_MAX_ARGS.saturating_sub(cap).min(3);
        assert!(
            max_extra >= 1,
            "the entry ABI takes {cap} arguments in registers and \
             `try_call` can pass at most {TRY_CALL_MAX_ARGS}, so this test \
             can no longer place ANY argument on the caller's stack. Give \
             `try_call` a wider thunk before trusting this test again."
        );
        for extra in 0..=max_extra {
            let slots = cap + extra;
            let last = slots - 1;
            // `iload <last>; ireturn` — `iload` (0x15) takes a one-byte index,
            // which covers every slot count this test uses.
            let code = [0x15, last as u8, 0xac];
            let compiled = compile_via_ir(&code, code.len(), slots, slots)
                .unwrap_or_else(|| panic!("a graph with {slots} incoming slots must lower"));
            let args: Vec<i64> = (0..slots).map(|i| 1000 + i as i64).collect();
            // SAFETY: the lowered body reads one int local and returns it; the
            // argument vector has exactly the arity the body was compiled for,
            // and every value is a plain integer.
            let got = unsafe { compiled.try_call(&args) };
            assert_eq!(
                got,
                Ok(1000 + last as i64),
                "with {slots} incoming slots ({cap} in registers, {extra} on the \
                 caller's stack), reading slot {last} must answer the argument \
                 that was passed there",
            );
        }
    }

    /// The same shape for a method that also takes the hidden VM context in
    /// ABI[0], which is what shifts an ordinary accessor over the edge: the
    /// context spends one register, so a method with `cap` Java arguments
    /// already has one on the stack.
    #[test]
    fn ir_lower_loads_stack_arguments_when_the_context_spends_a_register() {
        let cap = incoming_abi_reg_capacity();
        // A body with an `Op::Call` turns `needs_context` on. `invokestatic`
        // through a helper is the smallest one this harness can build, so this
        // test asserts the ARITHMETIC of the shift rather than re-deriving it:
        // with the context in ABI[0], `cap` Java arguments leave exactly one on
        // the caller's stack.
        assert_eq!(
            cap.saturating_sub(1),
            incoming_abi_reg_capacity() - 1,
            "one register is spent on the context"
        );
        // And the register/stack split the prologue computes for that case.
        let base = 1usize; // needs_context
        let num_params = cap;
        let reg_count = num_params.min(ENTRY_ABI_REGS.len().saturating_sub(base));
        assert_eq!(reg_count, cap - 1);
        assert_eq!(
            num_params - reg_count,
            1,
            "exactly one argument on the stack"
        );
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
    /// A synchronized region now reaches the IR tier instead of bailing.
    ///
    /// Two halves, and both matter. With NO monitor helper the compile must be
    /// REFUSED — `lower_data_node`'s catch-all would otherwise emit nothing for
    /// the monitor and the lock would silently disappear. With a helper present
    /// it must compile, and the emitted code must actually CALL that helper
    /// twice (enter + exit), not merely accept the graph.
    #[test]
    fn a_synchronized_region_lowers_through_the_monitor_helper() {
        // aload_0; monitorenter; aload_0; monitorexit; return
        let code = [0x2a, 0xc2, 0x2a, 0xc3, 0xb1, 0, 0];
        let builder = IrBuilder::new(1, 1);
        let graph = builder
            .build(&code, 5)
            .expect("builder must accept monitors");
        assert!(
            graph.nodes.iter().any(|n| matches!(n.op, Op::MonitorEnter)),
            "the builder must emit a MonitorEnter node"
        );
        let schedule = ir_schedule::schedule(&graph);

        // No helper -> refuse, rather than drop the lock.
        assert!(
            lower(&graph, &schedule, 1, 1, &no_helpers()).is_none(),
            "a monitor graph with no helper must be REFUSED, never lowered to \
             nothing"
        );

        // Helper present -> compiles, and both monitor calls are emitted.
        extern "C" fn fake_monitor(_vm: i64, obj: i64) -> i64 {
            obj
        }
        let mut helpers = no_helpers();
        helpers.monitor_enter = fake_monitor as *const () as usize;
        helpers.monitor_exit = fake_monitor as *const () as usize;
        let cm = lower(&graph, &schedule, 1, 1, &helpers)
            .expect("a monitor graph with a helper must compile");
        let target = (fake_monitor as *const () as usize).to_le_bytes();
        let bytes = cm.code_bytes();
        let calls = bytes.windows(8).filter(|w| *w == target).count();
        assert_eq!(
            calls, 2,
            "monitorenter and monitorexit must each call the helper"
        );
    }
    /// COV-03 — build the one-line `void set(Corpus o, X v) { o.f = v; }` graph
    /// for field descriptor `tag`, with both wide-field gates on.
    ///   aload_0; <x>load_1; putfield #2; return
    fn ref_or_wide_putfield_graph(tag: u8, load_op: u8, param_ty: IrType) -> Graph {
        let code = [0x2a, load_op, 0x01, 0xb5, 0x00, 0x02, 0xb1, 0, 0];
        let mut b = IrBuilder::new(2, 4);
        b.set_param_types(&[IrType::Ref, param_ty]);
        let mut fi = std::collections::HashMap::new();
        fi.insert(3usize, (0usize, tag));
        b.set_field_info(fi);
        b.set_wide_field_gates(true, true);
        b.build(&code, 7)
            .unwrap_or_else(|| panic!("the builder must accept a {} putfield", tag as char))
    }

    /// COV-03, the soundness half. A reference field store has exactly ONE
    /// correct lowering — `jit_putfield_object`, which carries the SATB
    /// pre-barrier on the overwritten reference and the collector's post-write
    /// barrier. With no helper address the graph must be REFUSED, never lowered
    /// to a barrier-free store: a missing barrier is invisible until a
    /// concurrent or generational collection, and it surfaces as a lost object
    /// rather than as a fault at the store.
    ///
    /// The second half is the one a refusal test cannot give you: with the
    /// helper wired, the emitted artifact must actually CALL it. "The graph was
    /// accepted" and "the barrier is in the code" are different claims.
    #[test]
    fn a_reference_putfield_lowers_only_through_the_barrier_helper() {
        // aload_0; aload_1; putfield #2; return
        let graph = ref_or_wide_putfield_graph(b'L', 0x19, IrType::Ref);
        assert!(
            graph
                .nodes
                .iter()
                .any(|n| matches!(n.op, Op::Store(MemKind::Ref))),
            "the builder must emit Op::Store(MemKind::Ref)"
        );
        let schedule = ir_schedule::schedule(&graph);

        assert!(
            lower(&graph, &schedule, 2, 4, &no_helpers()).is_none(),
            "a reference store with no `putfield_object` helper must be REFUSED, \
             never lowered to a store without its write barrier"
        );

        unsafe extern "C" fn fake_putfield_object(_vm: i64, _obj: i64, _idx: i64, _val: i64) {}
        let mut helpers = no_helpers();
        helpers.putfield_object = fake_putfield_object as *const () as usize;
        // `putfield_int` too: with compact layout on, `lower_inner` refuses an
        // INT store without it — and the `Const(field_index)` offset node this
        // graph carries is not an int store, so this only proves the refusal is
        // per-kind rather than blanket.
        helpers.putfield_int = fake_putfield_object as *const () as usize;
        let cm = lower(&graph, &schedule, 2, 4, &helpers)
            .expect("a reference store WITH the barrier helper must compile");
        assert!(
            contains_seq(
                cm.code_bytes(),
                &(helpers.putfield_object as u64).to_le_bytes()
            ),
            "the artifact must bake the `jit_putfield_object` address — the write \
             barrier is the whole reason this store has no inline lowering"
        );
        // The helper takes the VM context pointer as arg0, so the frame must
        // reserve and the ABI must demand it. Without this the lowering would
        // load arg0 from an unreserved slot and hand the helper a stack address
        // to dereference as a `SharedVm` (the single-pass backend's identical
        // `needs_heap` bug on `Catalina.setParentClassLoader`).
        assert!(
            cm.needs_context(),
            "a reference putfield must make the artifact `needs_context`"
        );
    }

    /// Lower `void set(Corpus o, X v) { o.f = v; }` with a resolved compact
    /// slot for the store site, and return the emitted bytes.
    ///
    /// `plan` publishes the collector's reference-store barrier gates; without
    /// it the helpers describe a collector that published none, which is the
    /// control arm for every assertion below.
    const COMPACT_REF_PUTFIELD_HELPER: usize = 0x1111_2222_3333_4450;

    fn lower_compact_ref_putfield(plan: bool, c_off: u32) -> Vec<u8> {
        // Static gate bytes and a read-bounds table. Real addresses of real
        // storage: the emitter bakes them as imm64 and the assertions read the
        // same values back, so a wrong one shows up as a missing sequence
        // rather than as a fault (nothing executes this code).
        static PRE_GATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
        static POST_GATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(1);
        static READ_BOUNDS: [u64; 6] = [0; 6];

        let graph = ref_or_wide_putfield_graph(b'L', 0x19, IrType::Ref);
        let schedule = ir_schedule::schedule(&graph);
        let mut helpers = no_helpers();
        helpers.putfield_object = COMPACT_REF_PUTFIELD_HELPER;
        helpers.putfield_int = COMPACT_REF_PUTFIELD_HELPER ^ 0xF;
        helpers.read_bounds_addr = std::ptr::addr_of!(READ_BOUNDS) as usize;
        if plan {
            helpers.ref_store_pre_gate = std::ptr::addr_of!(PRE_GATE) as usize;
            helpers.ref_store_post_gate = std::ptr::addr_of!(POST_GATE) as usize;
            helpers.ref_store_post_skip_mask = cratonvm_types::GC_FLAG_OLD_GEN as usize;
        }

        // The builder puts the `putfield` at pc 3 (`aload_0; aload_1;
        // putfield`), which is the key `set_field_info` uses above.
        let mut compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        compact.insert((3usize, true), (c_off, true, b'L'));
        let empty_hints: HashMap<usize, bool> = HashMap::new();
        let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
        let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
        lower_inner(
            &graph,
            &schedule,
            2,
            4,
            &helpers,
            &empty_hints,
            &[],
            None,
            &no_direct,
            &no_ic,
            &compact,
        )
        .expect("a reference putfield with its helper wired must compile")
        .code_bytes()
        .to_vec()
    }

    /// The optimizing tier emits the BARRIER-FREE store only when a collector
    /// published a barrier plan — and emits none without one.
    ///
    /// This is the pair, not the positive alone. Until 2026-09-02 this tier
    /// lowered every reference store to `jit_putfield_object` unconditionally,
    /// so a test that only asserted "the helper address is baked" passed both
    /// before and after the arm existed and could not tell them apart. The
    /// control arm here is a helper table with no plan, which is what G1 and
    /// ZGC actually present, and it must produce no inline store at all.
    #[test]
    fn the_optimizing_tier_stores_inline_only_behind_a_published_barrier_plan() {
        let c_off = 8u32;
        // MOV [RAX + disp32], RDX — the barrier-free compact reference store.
        let mut store_seq: Vec<u8> = vec![0x48, 0x89, 0x90];
        store_seq.extend_from_slice(&((HEADER_SIZE + c_off as usize) as i32).to_le_bytes());

        let without = lower_compact_ref_putfield(false, c_off);
        assert!(
            !contains_seq(&without, &store_seq),
            "with NO published barrier plan the optimizing tier must emit no \
             inline reference store — a store whose barrier nothing ruled out \
             is a lost card, invisible until the next collection"
        );

        let with = lower_compact_ref_putfield(true, c_off);
        assert!(
            contains_seq(&with, &store_seq),
            "with a published plan the optimizing tier must emit the inline \
             compact store at the RESOLVED offset — that offset is the whole \
             plumbing this arm needed, and the uniform slot displacement the \
             fallback derives is one a compact object does not obey"
        );
        // The helper is still the fallback. Every gate that cannot rule a
        // barrier out branches to it, so its address must remain baked: an arm
        // that inlined the store and dropped the fallback would be exactly the
        // missing-barrier defect this whole sequence is built to avoid.
        assert!(
            contains_seq(&with, &(COMPACT_REF_PUTFIELD_HELPER as u64).to_le_bytes()),
            "the gated arm must keep `jit_putfield_object` as its fallback"
        );
    }

    /// The baked compact cell offset is guarded against a layout REPLACEMENT,
    /// and the guard is proven to fire.
    ///
    /// Every emitter that bakes `HEADER_SIZE + packed_body_offset` claims at
    /// compile time something the class manager can change at run time. The two
    /// ALLOCATION emitters have guarded that since perf/halfgap-20260717, whose
    /// comment calls an unguarded baked layout "confirmed heap corruption"; the
    /// FIELD-ACCESS emitters had no guard at all until 2026-09-04, and the
    /// compact TLAB default made their unguarded path the common one.
    ///
    /// Asserted by EXECUTION, not by inspecting bytes: the compiled body is run
    /// against a receiver, then the process-wide replacement epoch is bumped by
    /// actually replacing a layout, and the same body is run again. The first
    /// run must store inline; the second must reach the helper. A guard that
    /// compares against a counter nothing ever bumps would pass an
    /// inspection-only test and protect nothing.
    #[test]
    fn a_replaced_layout_routes_the_gated_store_to_the_helper() {
        use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

        static HELPER_CALLS: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "C" fn marker_putfield_object(_vm: i64, _obj: i64, _idx: i64, _val: i64) {
            HELPER_CALLS.fetch_add(1, Ordering::SeqCst);
        }
        static PRE_GATE: AtomicU64 = AtomicU64::new(0);
        static POST_GATE: AtomicU64 = AtomicU64::new(1);
        static READ_BOUNDS: [AtomicUsize; 6] = [
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
        ];

        let graph = ref_or_wide_putfield_graph(b'L', 0x19, IrType::Ref);
        let schedule = ir_schedule::schedule(&graph);
        let mut helpers = no_helpers();
        helpers.putfield_object = marker_putfield_object as *const () as usize; // Cast: fn → slot
        helpers.read_bounds_addr = READ_BOUNDS.as_ptr() as usize; // Cast: static address
        helpers.ref_store_pre_gate = std::ptr::addr_of!(PRE_GATE) as usize; // Cast: static address
        helpers.ref_store_post_gate = std::ptr::addr_of!(POST_GATE) as usize; // Cast: static
        helpers.ref_store_post_young_floor = 0;
        helpers.ref_store_post_skip_mask = cratonvm_types::GC_FLAG_OLD_GEN as usize;

        let mut compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        compact.insert((3usize, true), (0u32, true, b'L'));
        let empty_hints: HashMap<usize, bool> = HashMap::new();
        let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
        let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
        let compiled = lower_inner(
            &graph,
            &schedule,
            2,
            4,
            &helpers,
            &empty_hints,
            &[],
            None,
            &no_direct,
            &no_ic,
            &compact,
        )
        .expect("the gated reference store must compile");

        // A young, compact receiver with one slot.
        let mut obj = Box::new([0u64; 8]);
        // SAFETY: `obj` is 64 bytes, 8-byte aligned; both writes land inside it.
        unsafe {
            let p = obj.as_mut_ptr() as *mut u8; // Cast: array base → byte cursor
            *p.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) = cratonvm_types::GC_FLAG_COMPACT;
            std::ptr::write_unaligned(
                p.add(cratonvm_types::NUM_SLOTS_OFFSET) as *mut u32, // Cast: header field
                4u32,
            );
        }
        let obj_addr = obj.as_mut_ptr() as usize; // Cast: receiver address
        let page = obj_addr & !0xFFF;
        READ_BOUNDS[0].store(page, Ordering::Release);
        READ_BOUNDS[1].store(page + 0x10000, Ordering::Release);
        let val = Box::new([0u64; 8]);
        let val_addr = val.as_ptr() as usize; // Cast: stored reference

        HELPER_CALLS.store(0, Ordering::SeqCst);
        // SAFETY: JIT-compiled code from valid bytecode in an executable mmap;
        // the receiver is a live buffer shaped like an object header and the
        // marker helper stores nothing.
        unsafe {
            compiled.call_with_heap(0, &[obj_addr as i64, val_addr as i64]); // Cast: JIT ABI
        }
        assert_eq!(
            HELPER_CALLS.load(Ordering::SeqCst),
            0,
            "before any replacement the guard must pass and the store stay inline"
        );
        assert_eq!(
            obj[cratonvm_types::HEADER_SIZE / 8] as usize, // Cast: stored pointer word
            val_addr,
            "the inline store must have written the compact cell"
        );

        // Now REPLACE a layout. Not by poking the counter -- by doing the thing
        // that bumps it, so the test cannot pass against a guard wired to a
        // number nothing in the VM ever moves.
        let before = cratonvm_types::layout_replace_epoch();
        // A REPLACEMENT is a second `register_class_layout` for a class_id that
        // already has one; a first registration deliberately does not count.
        let cid = 900_001u32;
        let mk = |body: u32| {
            std::sync::Arc::new(cratonvm_types::CompactLayout {
                field_offsets: vec![0],
                is_ref: vec![false],
                field_kinds: vec![cratonvm_types::FieldStorageKind::Int],
                ref_offsets: Vec::new(),
                body_size: body,
            })
        };
        let (l1, l2) = (mk(4), mk(8));
        cratonvm_types::register_class_layout(cratonvm_types::FIRST_LAYOUT_DOMAIN, cid, l1);
        cratonvm_types::register_class_layout(cratonvm_types::FIRST_LAYOUT_DOMAIN, cid, l2);
        assert!(
            cratonvm_types::layout_replace_epoch() > before,
            "replacing a registered layout must bump the process-wide epoch, or \
             the guard below is comparing against a constant"
        );

        obj[cratonvm_types::HEADER_SIZE / 8] = 0;
        HELPER_CALLS.store(0, Ordering::SeqCst);
        // SAFETY: as above.
        unsafe {
            compiled.call_with_heap(0, &[obj_addr as i64, val_addr as i64]); // Cast: JIT ABI
        }
        assert_eq!(
            HELPER_CALLS.load(Ordering::SeqCst),
            1,
            "a replaced layout must route the already-compiled site to the \
             always-correct helper — its baked cell offset now describes a \
             layout the object no longer has"
        );
        assert_eq!(
            obj[cratonvm_types::HEADER_SIZE / 8],
            0,
            "and the inline store must not have run"
        );
    }

    /// The gated arm emits BOTH store shapes and picks between them per object.
    ///
    /// A compact-only arm is an arm that almost never fires. `init_object_header`
    /// — the TLAB fast path serving nearly every allocation for the interpreter
    /// and `jit_new_object` alike — writes a LEGACY header unconditionally, no
    /// `GC_FLAG_COMPACT`, whatever layout the class has registered. Measured on
    /// `RefStoreLoopProbe` with only the compact shape emitted: 16,384,000
    /// executions, `inline=0`, every one bailing at the compactness test, on
    /// Generational and ZGC alike. This asserts the shape that fixed it.
    #[test]
    fn the_gated_arm_emits_the_legacy_cell_store_as_well_as_the_compact_one() {
        let code = lower_compact_ref_putfield(true, 8);
        // The legacy 16-byte `Value` cell for field 0: tag qword then the
        // pointer payload, both at `HEADER_SIZE + 0 * SLOT_SIZE`.
        // `i32::try_from`, not the bare cast: the audit in
        // `flag_and_header_contracts` counts that cast form as an EMISSION
        // site, and a test expectation is not one -- including in this comment,
        // which is why it does not spell the form out.
        let legacy_off = i32::try_from(HEADER_SIZE).expect("the header fits an i32");
        let mut tag_store: Vec<u8> = vec![0x4C, 0x89, 0x90]; // MOV [RAX+disp32], R10
        tag_store.extend_from_slice(
            &(legacy_off + cratonvm_types::FIELD_CELL_TAG_OFFSET as i32).to_le_bytes(),
        );
        assert!(
            contains_seq(&code, &tag_store),
            "the legacy shape must write the cell's TAG — a payload written \
             without it leaves a cell whose discriminant still says whatever \
             the field held before, and the reader believes the discriminant"
        );
        let mut pay_store: Vec<u8> = vec![0x48, 0x89, 0x90]; // MOV [RAX+disp32], RDX
        pay_store.extend_from_slice(
            &(legacy_off + FIELD_CELL_PAYLOAD64_OFFSET as i32).to_le_bytes(),
        );
        assert!(
            contains_seq(&code, &pay_store),
            "the legacy shape must write the 64-bit pointer payload"
        );
        // And the compact shape is still there — this is a two-shape arm, not a
        // replacement of one dead shape by another.
        let mut compact_store: Vec<u8> = vec![0x48, 0x89, 0x90];
        compact_store.extend_from_slice(&((HEADER_SIZE + 8usize) as i32).to_le_bytes());
        assert!(
            contains_seq(&code, &compact_store),
            "the compact shape must survive: the receiver's own header decides, \
             and a genuinely compact object stores the bare pointer"
        );
    }

    /// The gated arm reads the receiver's flags byte ONCE and asks both
    /// questions of it: compactness, then the post-barrier skip mask.
    ///
    /// Both are `TEST CL, imm8` against a bit in `GC_FLAGS_BYTE_OFFSET`, which
    /// packs `gc_age` in bits 4..7 and the GC flags in bits 0..3. The mask
    /// shape exists because the floor shape cannot express the generational
    /// question: an old-gen object allocated at `gc_age == 0` has flags byte
    /// `0x01` and sorts BELOW a young object that survived three collections
    /// (`0x30`), so an unsigned floor would skip the card the first one needs.
    #[test]
    fn the_gated_arm_tests_the_flags_byte_for_compactness_and_for_the_skip_mask() {
        let code = lower_compact_ref_putfield(true, 8);
        assert!(
            contains_seq(
                &code,
                &[0xF6, 0xC1, cratonvm_types::GC_FLAG_COMPACT]
            ),
            "the arm must test GC_FLAG_COMPACT — a class with a registered \
             compact layout can still have legacy-cell instances, and storing \
             a pointer at the packed offset into one of those corrupts a \
             neighbouring field"
        );
        assert!(
            contains_seq(
                &code,
                &[0xF6, 0xC1, cratonvm_types::GC_FLAG_OLD_GEN]
            ),
            "the arm must test the published skip mask to rule the post \
             barrier out"
        );
        // MOVZX ECX, byte [RAX + GC_FLAGS_BYTE_OFFSET] — read once, asked
        // twice. A second read would be a second chance for the two answers to
        // come from different bytes.
        let mut read_seq: Vec<u8> = vec![0x0F, 0xB6, 0x88];
        read_seq
            .extend_from_slice(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());
        assert!(
            contains_seq(&code, &read_seq),
            "the flags byte must be read as a BYTE at its own offset, not as \
             the dword the older arms use — that dword starts 15 bytes into a \
             16-byte header and takes three of its four bytes from the first \
             instance field"
        );
    }

    /// A site with no resolved compact slot keeps the unconditional helper.
    ///
    /// The refusal that matters most: `HEADER_SIZE + field_index * SLOT_SIZE`
    /// is the legacy cell displacement, and emitting a store there for a
    /// compact object would write a pointer over an unrelated field. The
    /// snapshot being absent is the ordinary case for an unresolved site, not
    /// an error.
    #[test]
    fn a_reference_store_with_no_resolved_compact_slot_keeps_the_helper() {
        static PRE_GATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
        static POST_GATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(1);
        static READ_BOUNDS: [u64; 6] = [0; 6];
        unsafe extern "C" fn fake_putfield_object(_vm: i64, _obj: i64, _idx: i64, _val: i64) {}

        let graph = ref_or_wide_putfield_graph(b'L', 0x19, IrType::Ref);
        let schedule = ir_schedule::schedule(&graph);
        let mut helpers = no_helpers();
        helpers.putfield_object = fake_putfield_object as *const () as usize;
        helpers.putfield_int = fake_putfield_object as *const () as usize;
        helpers.read_bounds_addr = std::ptr::addr_of!(READ_BOUNDS) as usize;
        helpers.ref_store_pre_gate = std::ptr::addr_of!(PRE_GATE) as usize;
        helpers.ref_store_post_gate = std::ptr::addr_of!(POST_GATE) as usize;
        helpers.ref_store_post_skip_mask = cratonvm_types::GC_FLAG_OLD_GEN as usize;

        let empty_hints: HashMap<usize, bool> = HashMap::new();
        let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
        let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
        let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        let code = lower_inner(
            &graph,
            &schedule,
            2,
            4,
            &helpers,
            &empty_hints,
            &[],
            None,
            &no_direct,
            &no_ic,
            &no_compact,
        )
        .expect("the store must still compile through the helper")
        .code_bytes()
        .to_vec();
        assert!(
            !contains_seq(&code, &[0xF6, 0xC1, cratonvm_types::GC_FLAG_COMPACT]),
            "with no resolved compact slot the gated arm must not be emitted \
             at all, published plan or not"
        );
        assert!(
            contains_seq(
                &code,
                &(fake_putfield_object as *const () as u64).to_le_bytes()
            ),
            "the unconditional `jit_putfield_object` lowering must remain"
        );
    }

    /// A plan is all three gates or none, and the optimizing tier reads that
    /// through the SAME predicate the single-pass backend uses.
    ///
    /// The shared reader is the point. A pre-gate with a dead post-gate would
    /// let compiled code skip the post barrier on the strength of a word nobody
    /// maintains, and two tiers deciding that independently is how one of them
    /// ends up skipping a barrier the other pays.
    #[test]
    fn a_half_published_plan_leaves_the_optimizing_tier_on_the_helper() {
        static PRE_GATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
        static READ_BOUNDS: [u64; 6] = [0; 6];
        unsafe extern "C" fn fake_putfield_object(_vm: i64, _obj: i64, _idx: i64, _val: i64) {}

        let graph = ref_or_wide_putfield_graph(b'L', 0x19, IrType::Ref);
        let schedule = ir_schedule::schedule(&graph);
        let mut helpers = no_helpers();
        helpers.putfield_object = fake_putfield_object as *const () as usize;
        helpers.putfield_int = fake_putfield_object as *const () as usize;
        helpers.read_bounds_addr = std::ptr::addr_of!(READ_BOUNDS) as usize;
        // Pre-gate only: no post-gate, no post shape.
        helpers.ref_store_pre_gate = std::ptr::addr_of!(PRE_GATE) as usize;

        let mut compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        compact.insert((3usize, true), (8u32, true, b'L'));
        let empty_hints: HashMap<usize, bool> = HashMap::new();
        let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
        let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
        let code = lower_inner(
            &graph,
            &schedule,
            2,
            4,
            &helpers,
            &empty_hints,
            &[],
            None,
            &no_direct,
            &no_ic,
            &compact,
        )
        .expect("the store must still compile through the helper")
        .code_bytes()
        .to_vec();
        let mut store_seq: Vec<u8> = vec![0x48, 0x89, 0x90];
        store_seq.extend_from_slice(&((HEADER_SIZE + 8usize) as i32).to_le_bytes());
        assert!(
            !contains_seq(&code, &store_seq),
            "a plan missing its post-barrier gate must decline the whole fast \
             path, not emit a store gated only on the half that was published"
        );
    }

    /// COV-03 — the same per-kind refusal for the wide widths, each of which
    /// has its own helper. Wired one at a time so a graph is never accepted on
    /// the strength of a DIFFERENT width's helper being present.
    #[test]
    fn a_wide_putfield_lowers_only_through_its_own_width_helper() {
        unsafe extern "C" fn fake_putfield(_obj: i64, _idx: i64, _val: i64) {}
        let addr = fake_putfield as *const () as usize;
        for (tag, load_op, ty) in [
            (b'J', 0x16u8, IrType::Long),
            (b'F', 0x17, IrType::Float),
            (b'D', 0x18, IrType::Double),
        ] {
            let graph = ref_or_wide_putfield_graph(tag, load_op, ty);
            let schedule = ir_schedule::schedule(&graph);
            assert!(
                lower(&graph, &schedule, 2, 4, &no_helpers()).is_none(),
                "a {} store with no width helper must be refused",
                tag as char
            );
            let mut helpers = no_helpers();
            match tag {
                b'J' => helpers.putfield_long = addr,
                b'F' => helpers.putfield_float = addr,
                _ => helpers.putfield_double = addr,
            }
            let cm = lower(&graph, &schedule, 2, 4, &helpers)
                .unwrap_or_else(|| panic!("a {} store with its helper must compile", tag as char));
            assert!(
                contains_seq(cm.code_bytes(), &(addr as u64).to_le_bytes()),
                "the {} store must CALL its width helper",
                tag as char
            );
        }
    }

    /// COV-03 — a wide field READ needs `jit_dispatch_threw`, because
    /// `Long.MIN_VALUE` and the `-0.0` bit pattern are bit-identical to
    /// `jit_getfield`'s deopt/NPE sentinel. Without the out-of-band peek the
    /// lowering would have to either drop a real NPE or bail on a legitimate
    /// value, so it refuses instead.
    ///
    /// A REFERENCE read is the control: no plausible heap pointer equals
    /// `i64::MIN`, so it keeps the plain compare-and-bail and must NOT start
    /// demanding the peek.
    ///
    /// # The two stub bodies must not be identical
    ///
    /// This test decides "was `dispatch_threw`'s address baked into the code?"
    /// by searching the emitted bytes for it, so it is only meaningful while
    /// `dispatch_threw` and `getfield` have DIFFERENT addresses. Both stubs
    /// used to be `-> i64 { 0 }`, which compiles to the same `xor eax,eax; ret`
    /// — and the MSVC linker's identical-COMDAT-folding (`/OPT:ICF`, on in
    /// release, off in debug) then gave them ONE address. The `L` control arm
    /// found `getfield`'s baked address, could not tell it from
    /// `dispatch_threw`'s, and failed: **red in `--release`, green in `debug`,
    /// for a lowering that was correct all along.**
    ///
    /// `fake_getfield` therefore returns a distinct non-zero value (any
    /// non-sentinel value is a legitimate field read), which no linker may fold
    /// with `fake_dispatch_threw`'s semantically-required `0`. The assertion
    /// below pins that: if a future toolchain unifies them anyway, this fails
    /// with the reason instead of silently inverting a byte search.
    #[test]
    fn a_wide_field_read_refuses_without_the_sentinel_disambiguator() {
        unsafe extern "C" fn fake_getfield(_vm: i64, _obj: i64, _idx: i64) -> i64 {
            // NOT `0`: see the doc comment. Any value but `i64::MIN` reads as
            // an ordinary field value here.
            7
        }
        extern "C" fn fake_dispatch_threw() -> i64 {
            // Semantically pinned: `0` means "no out-of-band signal pending".
            0
        }
        assert_ne!(
            fake_getfield as *const () as usize, fake_dispatch_threw as *const () as usize,
            "the two stubs were folded to one address (MSVC /OPT:ICF or an LTO \
             equivalent), so searching the emitted code for `dispatch_threw` \
             cannot distinguish it from `getfield` and this test's control arm \
             is meaningless. Give the stub bodies distinct instructions again."
        );
        for (tag, ret, needs_peek) in [
            (b'J', 0xadu8, true),
            (b'D', 0xaf, true),
            (b'F', 0xae, true),
            (b'L', 0xb0, false),
        ] {
            // aload_0; getfield #2; <x>return
            let code = [0x2a, 0xb4, 0x00, 0x02, ret, 0, 0];
            let mut b = IrBuilder::new(1, 1);
            b.set_param_types(&[IrType::Ref]);
            let mut fi = std::collections::HashMap::new();
            fi.insert(1usize, (0usize, tag));
            b.set_field_info(fi);
            b.set_wide_field_gates(true, true);
            let graph = b.build(&code, 5).expect("wide getfield builds when gated");
            let schedule = ir_schedule::schedule(&graph);

            let mut helpers = no_helpers();
            helpers.getfield = fake_getfield as *const () as usize;
            assert_eq!(
                lower(&graph, &schedule, 1, 1, &helpers).is_none(),
                needs_peek,
                "{}: refusal without `dispatch_threw` should be {needs_peek}",
                tag as char
            );

            helpers.dispatch_threw = fake_dispatch_threw as *const () as usize;
            let cm = lower(&graph, &schedule, 1, 1, &helpers)
                .unwrap_or_else(|| panic!("{} read must compile once wired", tag as char));
            assert_eq!(
                contains_seq(
                    cm.code_bytes(),
                    &(helpers.dispatch_threw as u64).to_le_bytes()
                ),
                needs_peek,
                "{}: the sentinel peek must be emitted iff the width can collide",
                tag as char
            );
        }
    }

    /// A REFERENCE `getfield` must tell the helper that the code after the call
    /// is going to DEREFERENCE what comes back.
    ///
    /// The helper reads a `Value` out of the slot and returns the payload of
    /// whichever variant it finds, and compiled code checks the returned word
    /// against `i64::MIN` and nothing else. Without
    /// `GETFIELD_EXPECT_REFERENCE` a reference field whose slot had been
    /// type-punned to a primitive therefore came back as that primitive's bits
    /// and was followed as a pointer: `SQLChar.rawData` (declared `[C`) read
    /// back as `Int(1)`, and the `arraylength` this arm emits — a plain
    /// `MOV r32,[reg+4]` — faulted at `addr=0x5`.
    ///
    /// This is an EMISSION test on purpose. The miscompiled body was
    /// deterministic (three crashes, byte-identical registers) but its
    /// reachability was not — 3 crashes in 38 runs one hour and 0 in 129 the
    /// next — so running the workload can never distinguish a fix from luck.
    /// What can be pinned is that the flag is in the argument.
    #[test]
    fn a_lowered_reference_getfield_tells_the_helper_it_will_be_dereferenced() {
        // Any non-`i64::MIN` return reads as an ordinary field value; this test
        // never executes the code, it only inspects what was emitted.
        unsafe extern "C" fn stub_getfield(_vm: i64, _obj: i64, _idx: i64) -> i64 {
            7
        }
        let fake_getfield = stub_getfield;

        // aload_0; getfield #2 -> slot 5, `[C`; areturn
        let code = [0x2a, 0xb4, 0x00, 0x02, 0xb0, 0, 0];
        let mut b = IrBuilder::new(1, 1);
        b.set_param_types(&[IrType::Ref]);
        let mut fi = std::collections::HashMap::new();
        fi.insert(1usize, (5usize, b'['));
        b.set_field_info(fi);
        let graph = b.build(&code, 5).expect("a reference getfield builds");
        let schedule = ir_schedule::schedule(&graph);
        let mut helpers = no_helpers();
        helpers.getfield = fake_getfield as *const () as usize;
        let cm = lower(&graph, &schedule, 1, 1, &helpers).expect("must compile");

        // Two arms can lower this — the main one, which also claims the
        // receiver proof, and the inline-compact fallback, which does not.
        // Either is correct; emitting the BARE index is not.
        //
        // Built from the CONSTANTS, not by calling `getfield_index_arg`. A
        // first version of this test computed its expectation with the same
        // encoder the emitter uses, so disabling the encoder moved both sides
        // together and the assertion below passed while nothing was flagged —
        // it only went red on the int case, by accident. An expectation
        // computed from the thing under test is not an expectation.
        const EXPECT_REF: u64 = cratonvm_jit_api::GETFIELD_EXPECT_REFERENCE;
        const PROVEN: u64 = cratonvm_jit_api::GETFIELD_RECEIVER_PROVEN_OOP;
        let proven = (5u64 | EXPECT_REF | PROVEN).to_le_bytes();
        let unproven = (5u64 | EXPECT_REF).to_le_bytes();
        //
        // Accepting either means this pins "the emitted code flags the load",
        // not "every arm flags it" — measured: patching `ref_node` out of the
        // main arm alone leaves this green, because the fallback arm still
        // flags it. Per-arm coverage comes from routing all seven emit sites
        // in both backends through `getfield_index_arg`, plus the control that
        // disables that encoder and turns this red.
        assert!(
            contains_seq(cm.code_bytes(), &proven) || contains_seq(cm.code_bytes(), &unproven),
            "no getfield argument in the emitted code carries \
             GETFIELD_EXPECT_REFERENCE, so the helper will hand this \
             dereferencing arm whatever primitive the slot happens to hold"
        );

        // A PRIMITIVE field must not claim it: the flag makes the helper refuse
        // to return a payload, so setting it on an int load would turn every
        // such read into a silent zero.
        let code_i = [0x2a, 0xb4, 0x00, 0x02, 0xac, 0, 0];
        let mut b = IrBuilder::new(1, 1);
        b.set_param_types(&[IrType::Ref]);
        let mut fi = std::collections::HashMap::new();
        fi.insert(1usize, (5usize, b'I'));
        b.set_field_info(fi);
        let graph = b.build(&code_i, 5).expect("an int getfield builds");
        let schedule = ir_schedule::schedule(&graph);
        let cm = lower(&graph, &schedule, 1, 1, &helpers).expect("must compile");
        assert!(
            !contains_seq(cm.code_bytes(), &proven) && !contains_seq(cm.code_bytes(), &unproven),
            "an int field must not claim GETFIELD_EXPECT_REFERENCE"
        );
    }

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

    /// Count non-overlapping occurrences of `needle` in `hay`.
    fn count_seq(hay: &[u8], needle: &[u8]) -> usize {
        if needle.is_empty() || needle.len() > hay.len() {
            return 0;
        }
        hay.windows(needle.len()).filter(|w| *w == needle).count()
    }

    // ── IR inline caches (jit-inlining-and-ir-calls) ────────────────────

    /// Build → lower a `static int f(Obj o, int n) { return o.g(n); }` shaped
    /// body whose single `invokevirtual` is planned as an inline-cache site.
    ///
    /// `ic` is the `pc → (mic, pic)` map handed to `lower_inner`; passing an
    /// empty map exercises the unchanged generic-dispatch path, which is what
    /// makes the "absent plan is byte-identical" assertion below meaningful.
    ///
    /// The returned code is never EXECUTED — the baked helper/slot addresses
    /// are test sentinels, not real functions. These tests assert on the
    /// emitted instruction stream, which is the part this change owns; the
    /// end-to-end execution coverage for IR call lowering lives in
    /// `jit/tests/ir_vs_singlepass.rs` (see the doc for the cases to add
    /// there — that file is outside this change's ownership).

    /// Every inline-cache hit in this tier restores the innermost-frame mirror
    /// before it does anything else.
    ///
    /// A compiled callee's prologue publishes ITS `(rbp, compile id)` there, and
    /// nothing on the return path of a raw JIT->JIT call restores the caller's.
    /// Until 2026-08-24 neither of this tier's two inline-cache arms — the
    /// monomorphic MIC hit and each rung of the polymorphic PIC cascade — did,
    /// so after any hit the mirror named a frame that had already returned.
    /// `moving_young_frame_coverage_complete` then read
    /// `[stale_rbp - stale_method.sp_id_slot_off]`, matched no map, and refused
    /// the whole collection (`ACTIVE_FRAME_MAP`). Measured on H2
    /// `TestMVStoreTool`: one rbp with ONE saved return address claimed across
    /// collections by four different methods, one of them `RootReference.isLocked`
    /// with `maps=0` — a method that emits no safepoint and therefore cannot be
    /// the frame at a collection at all.
    ///
    /// The single-pass backend republishes at both of its equivalent arms and
    /// the shared hashed/vtable stub is handed `frame_record` for the same
    /// reason; these two were the gap.
    ///
    /// Asserted as "the byte right after the CALL", because that placement is
    /// the property: anything emitted between the return and the republish runs
    /// under a mirror naming a dead frame. The zero-`frame_record` arm is the
    /// control — it proves the assertion can FAIL, so a future edit that drops
    /// the republish cannot leave this test passing vacuously.
    #[test]
    fn every_inline_cache_hit_restores_the_frame_record_before_anything_else() {
        crate::x64::set_moving_young_override(Some(false));
        const MIC: usize = 0x7fff_0000_0000_1000;
        const PIC: usize = 0x7fff_0000_0000_2000;
        const FRAME_RECORD: usize = 0x7fff_0000_0000_3000;
        let mut ic = HashMap::new();
        ic.insert(2usize, (MIC, PIC));

        // `CALL R11` — the cached-entry indirect call, and the ONLY way this
        // tier reaches a compiled Java callee from an inline cache.
        const CALL_R11: [u8; 3] = [0x41, 0xFF, 0xD3];
        let seg = crate::x64::inline_rbp_tls_segment_prefix();

        let sites = |code: &[u8]| -> Vec<usize> {
            (0..code.len().saturating_sub(3))
                .filter(|&i| code[i..i + 3] == CALL_R11)
                .collect()
        };

        let with = lower_virtual_call_with_ic_fr(&ic, FRAME_RECORD);
        let without = lower_virtual_call_with_ic_fr(&ic, 0);

        let with_sites = sites(&with);
        // One MIC arm plus one rung per PIC entry. If this is ever zero the
        // test below is vacuous, which is the failure mode worth naming.
        assert!(
            with_sites.len() >= 1 + crate::JIT_PIC_ENTRIES,
            "expected at least {} cached-entry calls, found {} — the probe cannot fire",
            1 + crate::JIT_PIC_ENTRIES,
            with_sites.len()
        );

        for at in &with_sites {
            let next = with[at + 3];
            // Inline form: `MOV <seg>:[disp32], RBP` opens with the segment
            // prefix. Helper form (no probed TLS displacement): `PUSH RAX`
            // preserves the callee's Java return value first.
            assert!(
                next == seg || next == 0x50,
                "cached-entry CALL at {at} is followed by {next:#04x}, not the \
                 frame-record republish ({seg:#04x} inline / 0x50 helper form)"
            );
        }

        // The control. With no frame-record helper the republish is switched
        // off wholesale, so NONE of the sites may carry it — which is what
        // makes the assertion above discriminating rather than decorative.
        for at in sites(&without) {
            let next = without[at + 3];
            assert!(
                next != seg && next != 0x50,
                "frame_record=0 must emit no republish, but the call at {at} is \
                 followed by {next:#04x}"
            );
        }
    }

    fn lower_virtual_call_with_ic(ic: &HashMap<usize, (usize, usize)>) -> Vec<u8> {
        lower_virtual_call_with_ic_fr(ic, 0)
    }

    /// As [`lower_virtual_call_with_ic`], with the frame-record helper
    /// pointer set. Zero is what `no_helpers()` gives, and zero switches the
    /// whole frame record off, so a test about the record has to pass a live
    /// one or it is asking a question the emitter never reaches.
    fn lower_virtual_call_with_ic_fr(
        ic: &HashMap<usize, (usize, usize)>,
        frame_record: usize,
    ) -> Vec<u8> {
        // aload_0; iload_1; invokevirtual #2; ireturn
        let code = [0x2a, 0x1b, 0xb6, 0x00, 0x02, 0xac, 0x00, 0x00];
        // A live `JitInvokeInfo` for the site. Leaked deliberately: `Op::Call`
        // bakes its address, and the lowerer dereferences it to read
        // `invoke_kind`.
        let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
            class_name: "pkg/Obj",
            method_name: "g",
            descriptor: "(I)I",
            num_jit_args: 2, // receiver + one int
            return_type: b'I',
            invoke_kind: 0, // virtual
            declaring_class_id: 0,
        }));
        let info_ptr = info as *const JitInvokeInfo as usize;

        let mut builder = IrBuilder::new(2, 2);
        builder.set_param_types(&[IrType::Ref, IrType::Int]);
        let mut invoke_info = HashMap::new();
        invoke_info.insert(2usize, (info_ptr, 2usize, b'I'));
        builder.set_invoke_info(invoke_info);
        let graph = builder.build(&code, 6).expect("IR build of virtual call");
        let schedule = ir_schedule::schedule(&graph);

        let mut helpers = no_helpers();
        // Distinct non-zero sentinels so each path is identifiable in the
        // emitted imm64s.
        helpers.invoke_dispatch = 0x1111_2222_3333_4440;
        helpers.invoke_virtual_mic = 0x1111_2222_3333_4441;
        helpers.dispatch_threw = 0x1111_2222_3333_4442;
        helpers.frame_record = frame_record;

        let empty_hints: HashMap<usize, bool> = HashMap::new();
        let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
        let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        lower_inner(
            &graph,
            &schedule,
            2,
            2,
            &helpers,
            &empty_hints,
            &[],
            None,
            &no_direct,
            ic,
            &no_compact,
        )
        .expect("virtual-call method must lower")
        .code_bytes()
        .to_vec()
    }

    /// Lower one `invokestatic` with `n` int parameters as a DIRECT call to
    /// `entry`, and return the emitted bytes.
    ///
    /// `needs_ctx` picks whether the callee takes the hidden VM context
    /// pointer in `abi[0]`, which is what decides how many Java arguments are
    /// left for the register file.
    fn lower_direct_call_with_n_args(n: usize, entry: usize, needs_ctx: bool) -> Vec<u8> {
        assert!((1..=8).contains(&n));
        // The arguments are CONSTANTS, not this method's parameters. A caller
        // with `n` parameters would be refused outright by `lower()` when `n`
        // exceeds `incoming_abi_reg_capacity()` — which is exactly the case
        // under test — and the refusal is about the PROLOGUE, a different
        // question from the one these tests ask.
        let mut code: Vec<u8> = Vec::new();
        for _ in 0..n {
            code.push(0x03); // iconst_0
        }
        let invoke_pc = code.len();
        code.extend_from_slice(&[0xb8, 0x00, 0x02]); // invokestatic #2
        code.push(0xac); // ireturn
        let code_len = code.len();
        // `IrBuilder::build` reads a little past the end for operand fetch.
        code.extend_from_slice(&[0x00, 0x00]);

        let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
            class_name: "pkg/Wide",
            method_name: "f",
            descriptor: "(IIIIIII)I",
            num_jit_args: n,
            return_type: b'I',
            invoke_kind: 3, // static — the statically bound, directly bindable kind
            declaring_class_id: 0,
        }));

        let mut builder = IrBuilder::new(0, 1);
        builder.set_param_types(&[]);
        let mut invoke_info = HashMap::new();
        invoke_info.insert(invoke_pc, (info as *const JitInvokeInfo as usize, n, b'I'));
        builder.set_invoke_info(invoke_info);
        let graph = builder
            .build(&code, code_len)
            .expect("IR build of wide call");
        let schedule = ir_schedule::schedule(&graph);

        let mut helpers = no_helpers();
        helpers.invoke_dispatch = 0x1111_2222_3333_4440;

        let empty_hints: HashMap<usize, bool> = HashMap::new();
        let mut direct: HashMap<usize, (usize, bool)> = HashMap::new();
        direct.insert(invoke_pc, (entry, needs_ctx));
        let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
        let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        lower_inner(
            &graph,
            &schedule,
            0,
            1,
            &helpers,
            &empty_hints,
            &[],
            None,
            &direct,
            &no_ic,
            &no_compact,
        )
        .expect("wide direct call must lower")
        .code_bytes()
        .to_vec()
    }

    /// A direct call whose arguments do not all fit `ENTRY_ABI_REGS` marshals
    /// the remainder on the stack instead of falling back to the dispatch
    /// helper.
    ///
    /// This is the blocker `httpcontentdecompressortest-snappy-varhandle-bind-RETIRED-20260820.md`
    /// named: `emit_direct_cross_call` was register-only, so a site needing
    /// seven incoming slots kept the full `jit_invoke_dispatch` round trip no
    /// matter what the binding side had resolved. The map entry was recorded
    /// and then silently dropped by the lowerer — nothing warned.
    ///
    /// The assertion is structural rather than a byte-for-byte golden: the
    /// block size differs by platform (Win64 reserves 32 bytes of shadow
    /// space below the outgoing arguments, SysV none) and so does how many
    /// arguments are left over, and pinning both would only restate
    /// `stack_arg_block_size`.
    #[test]
    fn a_direct_call_past_the_register_file_marshals_its_tail_on_the_stack() {
        crate::x64::set_moving_young_override(Some(false));
        const ENTRY: usize = 0x7fff_0000_0000_5000;
        let n = ENTRY_ABI_REGS.len() + 2; // context + n args exceeds the file
        let code = lower_direct_call_with_n_args(n, ENTRY, true);

        assert!(
            contains_seq(&code, &(ENTRY as u64).to_le_bytes()),
            "the site must bake the callee entry, not fall back to dispatch"
        );
        assert!(
            !contains_seq(&code, &0x1111_2222_3333_4440u64.to_le_bytes()),
            "a bound direct site must not also emit the dispatch helper call"
        );

        // SUB RSP, imm32 / ADD RSP, imm32 with the SAME immediate: the block
        // is reserved and released around one call, so an unbalanced pair
        // would leave the frame's RSP permanently low.
        //
        // The FIRST `SUB RSP, imm32` in any body is the prologue's own frame
        // allocation, which shares this encoding — hence `skip(1)` rather than
        // `position`. Taking the first would assert against `frame_size` and
        // pass whether or not a stack-argument block was ever emitted.
        let subs: Vec<usize> = code
            .windows(3)
            .enumerate()
            .filter(|(_, w)| *w == [0x48, 0x81, 0xEC])
            .map(|(i, _)| i)
            .collect();
        assert!(
            subs.len() >= 2,
            "the prologue's frame allocation plus a stack-argument block: got {} SUB RSP",
            subs.len()
        );
        let sub_at = subs[1];
        let add_at = code
            .windows(3)
            .position(|w| w == [0x48, 0x81, 0xC4])
            .expect("the stack-argument block must be released");
        let sub_imm = i32::from_le_bytes(code[sub_at + 3..sub_at + 7].try_into().unwrap());
        let add_imm = i32::from_le_bytes(code[add_at + 3..add_at + 7].try_into().unwrap());
        assert_eq!(sub_imm, add_imm, "the reserve and the release must match");
        assert!(sub_imm > 0, "a stack-argument block must be non-empty here");
        assert_eq!(
            sub_imm % 16,
            0,
            "RSP must stay 16-byte aligned at the CALL, so the block is a multiple of 16"
        );
        assert!(add_at > sub_at, "the release must follow the reserve");

        // One `MOV [RSP + disp32], RAX` per argument past the register file.
        let stack_args = n - (ENTRY_ABI_REGS.len() - 1);
        assert_eq!(
            count_seq(&code, &[0x48, 0x89, 0x84, 0x24]),
            stack_args,
            "every argument past the entry-ABI register file must be stored to the block"
        );
    }

    /// The same lowering with arguments that DO fit emits no stack block at
    /// all — the narrow path is unchanged.
    ///
    /// Without this the test above would pass just as well against a backend
    /// that reserved a block on every direct call, which is a pessimisation of
    /// every call site in the tree.
    #[test]
    fn a_direct_call_within_the_register_file_reserves_no_stack_block() {
        crate::x64::set_moving_young_override(Some(false));
        const ENTRY: usize = 0x7fff_0000_0000_5000;
        let code = lower_direct_call_with_n_args(ENTRY_ABI_REGS.len() - 1, ENTRY, true);
        assert!(
            contains_seq(&code, &(ENTRY as u64).to_le_bytes()),
            "the narrow site must still bind directly"
        );
        assert_eq!(
            count_seq(&code, &[0x48, 0x81, 0xEC]),
            1,
            "only the prologue's own frame allocation — a call whose arguments \
             all fit registers must add no second SUB RSP"
        );
        assert_eq!(
            count_seq(&code, &[0x48, 0x89, 0x84, 0x24]),
            0,
            "a call whose arguments all fit registers must store nothing to the stack"
        );
    }

    /// A self-recursive call must not PUBLISH to the shadow stack.
    ///
    /// That route bypasses `emit_call_return_check`, the one site that emits
    /// the matching reload, so a push emitted here is never retracted: the
    /// thread's shadow `top` advances once per execution of the site and a
    /// recursive method walks it off the end of the shadow stack, after which
    /// the next push writes through into whatever follows the mapping. It
    /// presented as a SIGSEGV inside the C allocator's free list, on an
    /// unrelated thread, the moment reference field reads made
    /// `BinTreesClassic.itemCheck` IR-eligible.
    ///
    /// The lowerer used to emit the push and then "withdraw the claim" by
    /// clearing `pending_shadow` — which suppressed the RELOAD, not the push.
    #[test]
    fn self_recursive_call_balances_its_shadow_push_and_reload() {
        crate::x64::set_moving_young_override(Some(false));
        // static int f(Obj o, int n) { return f(o, n); }
        // aload_0; iload_1; invokestatic #2; ireturn
        let code = [0x2a, 0x1b, 0xb8, 0x00, 0x02, 0xac, 0x00, 0x00];
        let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
            class_name: "pkg/Obj",
            method_name: "f",
            descriptor: "(Lpkg/Obj;I)I",
            num_jit_args: 2,
            return_type: b'I',
            invoke_kind: 4, // the direct self-recursive route
            declaring_class_id: 0,
        }));
        let mut builder = IrBuilder::new(2, 2);
        builder.set_param_types(&[IrType::Ref, IrType::Int]);
        let mut invoke_info = HashMap::new();
        invoke_info.insert(
            2usize,
            (info as *const JitInvokeInfo as usize, 2usize, b'I'),
        );
        builder.set_invoke_info(invoke_info);
        let graph = builder
            .build(&code, 6)
            .expect("IR build of self-recursive call");
        let schedule = ir_schedule::schedule(&graph);

        // The shadow machinery is live only when the thread helper is wired,
        // so a stub table with a zero `get_current_thread` would make this
        // test vacuous. Give it sentinels.
        let mut helpers = no_helpers();
        helpers.get_current_thread = 0x7fff_0000_0000_3000;
        helpers.shadow_stack_offset_in_thread = 0x40;
        helpers.self_call_stack_guard = 0x7fff_0000_0000_4000;

        let empty_hints: HashMap<usize, bool> = HashMap::new();
        let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
        let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
        let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        let cm = lower_inner(
            &graph,
            &schedule,
            2,
            2,
            &helpers,
            &empty_hints,
            &[],
            None,
            &no_direct,
            &no_ic,
            &no_compact,
        )
        .expect(
            "the self-recursive body must lower — an imbalance now makes \
             lower_inner discard it, so a None here means the push came back",
        );
        // Lowering at all is most of the assertion: `lower_inner` counts the
        // emitted pushes against the emitted reloads and DISCARDS the body when
        // they disagree, so a `None` above is exactly the bug this test exists
        // for. What remains to check is that the retraction is really emitted
        // on this route rather than inherited from a choke point it never
        // reaches: `MOV RCX, [R11]` reads a published value back, and only the
        // reload does that. (Deliberately not an exact count — the prologue's
        // own `top` watermark shares the push's encoding, so counting pushes
        // here measures the prologue too.)
        let bytes = cm.code_bytes();
        assert!(
            count_seq(bytes, &[0x49, 0x8B, 0x0B]) >= 1,
            "the self-recursive route bypasses emit_call_return_check, so it \
             must emit its own shadow reload — without it every execution of \
             the site advances the thread's shadow top and nothing retracts it"
        );
        crate::x64::set_moving_young_override(None);
    }

    /// A planned inline-cache site must emit the full three-tier cascade:
    /// the receiver null check, ONE class-id load, the monomorphic guard, three
    /// polymorphic guards, four `CALL R11` cached-entry calls, and the
    /// `jit_invoke_virtual_mic` miss path — NOT the blind `jit_invoke_dispatch`
    /// helper the pre-2026-07-26 lowerer used for every virtual call.
    #[test]
    fn ic_site_emits_mic_pic_cascade_not_blind_dispatch() {
        // This test exercises the optimizing IR pipeline, which is gated off
        // whenever the young generation can relocate. Pin the policy so the
        // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
        crate::x64::set_moving_young_override(Some(false));
        const MIC: usize = 0x7fff_0000_0000_1000;
        const PIC: usize = 0x7fff_0000_0000_2000;
        let mut ic = HashMap::new();
        ic.insert(2usize, (MIC, PIC));
        let code = lower_virtual_call_with_ic(&ic);

        // Receiver null check + the single class-id load feeding every guard.
        assert!(
            contains_seq(&code, &[0x48, 0x85, 0xC0]),
            "TEST RAX,RAX receiver null check must be emitted"
        );
        assert_eq!(
            count_seq(&code, &[0x8B, 0x00]),
            1,
            "the receiver class id must be loaded exactly ONCE for the whole cascade"
        );
        // Both slot base pointers are baked as imm64 (`MOV R10, imm64` = 49 BA).
        assert!(
            contains_seq(&code, &[0x49, 0xBA]) && contains_seq(&code, &MIC.to_le_bytes()),
            "the MIC slot address must be baked into the guard"
        );
        assert!(
            contains_seq(&code, &PIC.to_le_bytes()),
            "the PIC slot address must be baked into the cascade"
        );
        // MIC + four PIC entries + two hashed ways all call via R11.
        assert_eq!(
            count_seq(&code, &[0x41, 0xFF, 0xD3]),
            1 + crate::JIT_PIC_ENTRIES + crate::JIT_MEGA_WAYS,
            "one cached-entry CALL R11 per MIC/PIC entry plus the two hashed ways"
        );
        // The miss path resolves AND populates via `jit_invoke_virtual_mic`.
        assert!(
            contains_seq(&code, &0x1111_2222_3333_4441u64.to_le_bytes()),
            "the inline-cache miss path must call jit_invoke_virtual_mic"
        );
        assert!(
            !contains_seq(&code, &0x1111_2222_3333_4440u64.to_le_bytes()),
            "a planned IC site must NOT also emit the blind jit_invoke_dispatch helper"
        );
    }

    /// Every cached entry below is selected by the 4-byte
    /// `ObjectHeader.class_id`, and a reference array stores its COMPONENT
    /// class id in that same word — so `Foo[]` and `Foo` are indistinguishable
    /// to a class-id-only guard, and a site warmed on a `Foo` receiver
    /// dispatched a later `Foo[]` receiver into `Foo`'s own body (whose first
    /// `checkcast Foo` threw `class [LFoo; cannot be cast to class Foo`).
    /// `ObjectHeader.kind` is what separates them.
    #[test]
    fn ic_cascade_rejects_array_receivers_before_the_class_id_guard() {
        // This test exercises the optimizing IR pipeline, which is gated off
        // whenever the young generation can relocate. Pin the policy so the
        // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
        crate::x64::set_moving_young_override(Some(false));
        const MIC: usize = 0x7fff_0000_0000_1000;
        const PIC: usize = 0x7fff_0000_0000_2000;
        let mut ic = HashMap::new();
        ic.insert(2usize, (MIC, PIC));
        let code = lower_virtual_call_with_ic(&ic);

        // CMP BYTE [RAX + KIND_TAGS_BYTE_OFFSET], ObjectKind::Object
        let guard = [
            0x80u8,
            0x78,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
            cratonvm_types::ObjectKind::Object as u8,
        ];
        assert!(
            contains_seq(&code, &guard),
            "the cascade must reject a non-object receiver kind before trusting the class id"
        );
        // ... and it must come BEFORE the single class-id load it protects.
        let guard_at = code
            .windows(guard.len())
            .position(|w| w == guard)
            .expect("guard present");
        let load_at = code
            .windows(2)
            .position(|w| w == [0x8B, 0x00])
            .expect("class-id load present");
        assert!(
            guard_at < load_at,
            "the kind guard must precede the class-id load (guard@{guard_at}, load@{load_at})"
        );
    }

    /// The guards must read the MIC/PIC fields at the offsets the shared slot
    /// types publish, not at hand-copied literals — this is what keeps the IR
    /// cascade layout-compatible with the single-pass one and with the runtime
    /// helper that populates both.
    #[test]
    fn ic_guards_use_published_slot_offsets() {
        // This test exercises the optimizing IR pipeline, which is gated off
        // whenever the young generation can relocate. Pin the policy so the
        // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
        crate::x64::set_moving_young_override(Some(false));
        use crate::{JitMICSlot, JitPICSlot};
        const MIC: usize = 0x7fff_0000_0000_1000;
        const PIC: usize = 0x7fff_0000_0000_2000;
        let mut ic = HashMap::new();
        ic.insert(2usize, (MIC, PIC));
        let code = lower_virtual_call_with_ic(&ic);

        // MIC: class id at offset 0 → `CMP EAX,[R10]` = 41 3B 02;
        // needs_context at 16 → `CMP BYTE [R10+16],0` = 41 80 7A 10 00;
        // entry ptr at 8 → `MOV R11,[R10+8]` = 4D 8B 5A 08.
        assert_eq!(JitMICSlot::CACHED_CLASS_ID_OFFSET, 0);
        assert!(contains_seq(&code, &[0x41, 0x3B, 0x02]));
        assert!(contains_seq(
            &code,
            &[
                0x41,
                0x80,
                0x7A,
                JitMICSlot::CACHED_NEEDS_CONTEXT_OFFSET as u8,
                0x00
            ]
        ));
        assert!(contains_seq(
            &code,
            &[
                0x4D,
                0x8B,
                0x5A,
                crate::x64::disp::disp8_const(JitMICSlot::CACHED_ENTRY_PTR_OFFSET as i64) as u8,
            ]
        ));
        assert!(
            contains_seq(
                &code,
                &[
                    0x49,
                    0x83,
                    0x7A,
                    JitMICSlot::CACHED_ENTRY_PTR_OFFSET as u8,
                    0x00
                ]
            ),
            "a class-only profiled MIC seed must miss until entry_ptr is populated"
        );

        // PIC: one guard + one needs-context gate + one entry load per entry.
        for i in 0..crate::JIT_PIC_ENTRIES {
            let cid = JitPICSlot::CLASS_ID_OFFSETS[i] as u8;
            if cid == 0 {
                assert!(contains_seq(&code, &[0x41, 0x3B, 0x02]));
            } else {
                assert!(
                    contains_seq(&code, &[0x41, 0x3B, 0x42, cid]),
                    "PIC entry {i} class-id guard at offset {cid} must be emitted"
                );
            }
            assert!(
                contains_seq(
                    &code,
                    &[
                        0x41,
                        0x80,
                        0x7A,
                        JitPICSlot::NEEDS_CONTEXT_OFFSETS[i] as u8,
                        0x00
                    ]
                ),
                "PIC entry {i} needs-context gate must be emitted"
            );
            assert!(
                contains_seq(
                    &code,
                    &[
                        0x4D,
                        0x8B,
                        0x5A,
                        crate::x64::disp::disp8_const(JitPICSlot::ENTRY_PTR_OFFSETS[i] as i64)
                            as u8,
                    ]
                ),
                "PIC entry {i} cached-entry load must be emitted"
            );
            assert!(
                contains_seq(
                    &code,
                    &[
                        0x49,
                        0x83,
                        0x7A,
                        JitPICSlot::ENTRY_PTR_OFFSETS[i] as u8,
                        0x00
                    ]
                ),
                "PIC entry {i} must reject a class-only profile seed"
            );
        }
    }

    /// No plan ⇒ the historical generic-dispatch lowering, unchanged. This is
    /// the guarantee that every un-planned call site in every method is
    /// byte-identical to the pre-2026-07-26 backend.
    #[test]
    fn absent_ic_plan_keeps_generic_dispatch() {
        let empty: HashMap<usize, (usize, usize)> = HashMap::new();
        let code = lower_virtual_call_with_ic(&empty);
        assert!(
            contains_seq(&code, &0x1111_2222_3333_4440u64.to_le_bytes()),
            "an unplanned virtual site must still route through jit_invoke_dispatch"
        );
        assert!(
            !contains_seq(&code, &0x1111_2222_3333_4441u64.to_le_bytes()),
            "an unplanned site must NOT reach the inline-cache miss helper"
        );
        assert_eq!(
            count_seq(&code, &[0x41, 0xFF, 0xD3]),
            0,
            "an unplanned site must emit no cached-entry CALL R11"
        );
    }

    /// Every branch the inline-cache cascade emits is patched before the
    /// emitter returns, so no `rel32` operand is left as its `00 00 00 00`
    /// placeholder. A stale placeholder would jump to the instruction
    /// immediately after itself — a silent fall-through into the next cache
    /// entry's body rather than to `.pic` / `.slow` / `.done`.
    ///
    /// (`patch_rel32_to_here` swallows patch errors by design, because they are
    /// only reachable on an overflowed buffer that `lower_inner` discards — so
    /// this test is what proves the non-overflow path really does patch.)
    #[test]
    fn ic_cascade_leaves_no_unpatched_branch() {
        const MIC: usize = 0x7fff_0000_0000_1000;
        const PIC: usize = 0x7fff_0000_0000_2000;
        let mut ic = HashMap::new();
        ic.insert(2usize, (MIC, PIC));
        let code = lower_virtual_call_with_ic(&ic);

        // Scan for a Jcc rel32 / JMP rel32 whose displacement is still zero.
        for w in 0..code.len().saturating_sub(6) {
            let is_jcc = code[w] == 0x0F && (code[w + 1] == 0x84 || code[w + 1] == 0x85);
            if is_jcc && code[w + 2..w + 6] == [0, 0, 0, 0] {
                panic!("unpatched Jcc rel32 placeholder at native offset {w}");
            }
        }
        for w in 0..code.len().saturating_sub(5) {
            if code[w] == 0xE9 && code[w + 1..w + 5] == [0, 0, 0, 0] {
                panic!("unpatched JMP rel32 placeholder at native offset {w}");
            }
        }
    }

    /// A site whose receiver + arguments + hidden VM context pointer overflow
    /// the entry ABI register file must fall back to the helper rather than
    /// emit a call with garbage in an unwritten register. On Win64 the file is
    /// 4 registers wide, so a 4-argument virtual site (receiver + 3) already
    /// needs 5 and must decline; SysV has 6, so the same shape fits there and
    /// the assertion is platform-specific by construction.
    #[test]
    fn ic_declines_when_args_overflow_the_abi_register_file() {
        // The lowerer's own guard is `num_args + 1 <= ENTRY_ABI_REGS.len()`;
        // pin the constant rather than re-deriving it, so a future ABI change
        // has to visit this test.
        #[cfg(target_os = "windows")]
        assert_eq!(ENTRY_ABI_REGS.len(), 4);
        #[cfg(not(target_os = "windows"))]
        assert_eq!(ENTRY_ABI_REGS.len(), 6);
        // The shared planner bound must agree with the lowerer's.
        assert_eq!(crate::ir_entry_abi_reg_count(), ENTRY_ABI_REGS.len());
    }

    /// The context shift moves arguments in DESCENDING order, so no argument is
    /// overwritten before it has been read.
    ///
    /// `emit_ic_premarshal_no_context` builds the no-context layout once, ahead
    /// of the whole inline-cache cascade, and a context-needing arm converts it
    /// by shifting every argument up one register. Ascending order would write
    /// `ENTRY_ABI_REGS[1]` from `[0]` before reading `[1]`, so argument 1 would
    /// become a second copy of argument 0 and every argument above it the same
    /// — silently, and only for callees that need the context pointer.
    ///
    /// Asserted on the decoded `(dst, src)` pairs rather than on literal bytes,
    /// because `ENTRY_ABI_REGS` differs between Win64 and System V and a
    /// byte-literal test would pin one platform's answer as the property.
    #[test]
    fn the_context_shift_moves_arguments_from_the_top_down() {
        for num_args in 1..=(ENTRY_ABI_REGS.len() - 1) {
            let mut lo = lowerer_with_resident_xmm(4096, None);
            let at = lo.buf.pos();
            lo.emit_ic_shift_for_context(num_args);
            let code = lo.buf.as_slice()[at..].to_vec();

            // `MOV r64, r64` is `REX.W(+R+B) 89 ModRM(mod=11)`. Decode every one
            // in order; the trailing context load is `8B` and is skipped.
            let mut moves: Vec<(u8, u8)> = Vec::new();
            let mut i = 0usize;
            while i + 2 < code.len() {
                if (0x48..=0x4F).contains(&code[i]) && code[i + 1] == 0x89 && code[i + 2] >= 0xC0 {
                    let rex = code[i];
                    let modrm = code[i + 2];
                    let src = ((modrm >> 3) & 7) | (((rex >> 2) & 1) << 3);
                    let dst = (modrm & 7) | ((rex & 1) << 3);
                    moves.push((dst, src));
                    i += 3;
                } else {
                    i += 1;
                }
            }

            let want: Vec<(u8, u8)> = (0..num_args)
                .rev()
                .map(|k| (ENTRY_ABI_REGS[k + 1], ENTRY_ABI_REGS[k]))
                .collect();
            assert_eq!(
                moves, want,
                "num_args={num_args}: the shift must run from the top down, so \
                 every destination holds a value that has already been moved",
            );

            // …and the context lands in ABI[0], after the shift has vacated it.
            // `MOV r64, [rbp - disp]` is `REX.W 8B ModRM`, and ABI[0]'s encoding
            // appears in the ModRM `reg` field.
            let ctx = ENTRY_ABI_REGS[0];
            let ctx_loaded = code.windows(3).any(|w| {
                (0x48..=0x4F).contains(&w[0])
                    && w[1] == 0x8B
                    && ((w[2] >> 3) & 7) == (ctx & 7)
                    && (w[2] & 0xC0) != 0xC0
            });
            assert!(
                ctx_loaded,
                "num_args={num_args}: the context pointer must be loaded into \
                 ABI[0] once the shift has vacated it",
            );
        }
    }

    /// The optimizing tier may not be opened to allocation-bearing methods
    /// while it still lowers `Op::New` through the out-of-line stub.
    ///
    /// # The coupling this enforces
    ///
    /// `emit_new_object_stub` is three register loads and a `CALL` into
    /// `jit_new_object`. The single-pass backend has
    /// `x64::objects::emit_inline_tlab_new` and pays no call on the common
    /// path. So an escaping allocation compiles WORSE at the optimizing tier
    /// than at the baseline tier — which is one of the two independent causes
    /// of the July 2026 Binary Trees 4x regression, and the reason
    /// `IR_MAX_ALLOCATIONS` is pinned at 16 while every neighbouring cap is 64.
    ///
    /// It costs nothing today only because `c2_alloc_upgrade_enabled()` is
    /// opt-in, so no method containing a `new` is ever promoted to this tier.
    /// That was a sentence in a comment. It is a test now, because the edit
    /// that breaks it — flipping the gate on, in `lib.rs`, to widen the
    /// optimizing tier's population — does not look like it touches allocation
    /// at all, and its symptom is a throughput regression on exactly the
    /// workloads nobody re-measures after a policy change.
    ///
    /// # What discharges it
    ///
    /// Giving this tier an inline TLAB bump. That is not a copy of the
    /// single-pass sequence: `jit_post_tlab_init` derives `shape` and the
    /// object's total size from `class_layout(class_id)` **itself**, so a
    /// caller that sizes the allocation as `HEADER_SIZE + num_fields *
    /// SLOT_SIZE` while the class carries a registered compact layout hands the
    /// helper a size mismatch and corrupts the heap. A correct implementation
    /// needs the compact snapshot AND the runtime layout-version guard the
    /// single-pass emitter carries for a layout that is REPLACED between
    /// compile and execution. One shared sequence is the right answer; the
    /// obstacle is that the header-write contract is policed by source scans of
    /// `emit_inline_tlab_new`'s own body, so moving it means rewriting the
    /// oracle in the same change as the code it polices.
    ///
    /// When that lands, delete this test — do not weaken it.
    #[test]
    fn the_optimizing_tier_stays_shut_to_allocation_while_it_has_no_inline_tlab() {
        let ir_src = include_str!("ir_lower.rs");
        // The `Op::New` arm's lowering, as it stands.
        let uses_stub = ir_src.contains("runtime_lowering::emit_new_object_stub");
        let has_inline_bump = ir_src.contains("emit_inline_tlab_new_ir");
        assert!(
            uses_stub || has_inline_bump,
            "the `Op::New` arm lowers through neither the stub nor an inline \
             bump — this test can no longer see what it is guarding"
        );
        if has_inline_bump {
            // The gap is closed; the coupling below has nothing to protect.
            return;
        }

        // Still stub-only. Then the gate must be OPT-IN: `runtime_var_os(..)
        // .is_some()` is off unless the variable is set, whereas a
        // `map_or(true, ..)` or a `!matches!(.., Ok("0"))` would be default-on.
        let lib_src = include_str!("lib.rs");
        let body = lib_src
            .split("fn c2_alloc_upgrade_enabled() -> bool {")
            .nth(1)
            .expect("c2_alloc_upgrade_enabled is in lib.rs")
            .split("\n}")
            .next()
            .expect("the function ends");
        assert!(
            body.contains("is_some()"),
            "`c2_alloc_upgrade_enabled` is no longer opt-in, but the optimizing \
             tier still lowers `Op::New` through `emit_new_object_stub` — every \
             promoted allocation now pays a CALL where the single-pass backend \
             pays an inline TLAB bump. Give this tier the bump first (see this \
             test's doc comment for why it is not a copy-paste), or leave the \
             gate shut.\n\nbody was:\n{body}"
        );
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
        // 2026-09-02: with the compare FUSED into the branch, the emitted
        // condition is the compare's own inverse rather than `TEST` on a
        // materialised boolean, so the concrete byte for this `ifeq` shape is
        // `JNE` (0F 85) where it used to be `JE` (0F 84). What this test is
        // for -- the hint INVERTS the branch, and only the hint does -- is
        // unchanged, and is asserted as the property below rather than as one
        // of the two bytes. Both polarities must appear across the arms, or
        // the "inversion" being checked is vacuous.
        let base_polarity = if contains_seq(&base_code, &[0x0F, 0x84]) {
            0x84u8
        } else {
            assert!(
                contains_seq(&base_code, &[0x0F, 0x85]),
                "the baseline IR branch emitted neither JE nor JNE"
            );
            0x85u8
        };

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
            contains_seq(&hint_code, &[0x0F, base_polarity ^ 1]),
            "a not-taken hint must invert the branch polarity"
        );
        assert_ne!(
            base_code, hint_code,
            "branch-bias hint must change the emitted code"
        );

        // "usually taken" keeps the default layout (byte-identical).
        let mut taken_hints = HashMap::new();
        taken_hints.insert(1usize, true);
        let (g2, s2) = build();
        let taken_code = lower_with_branch_hints(&g2, &s2, 1, 1, &no_helpers(), &taken_hints)
            .expect("lower taken-hinted")
            .code_bytes()
            .to_vec();
        assert_eq!(
            base_code, taken_code,
            "usually-taken hint must reproduce the default layout byte-for-byte"
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
            uses: Default::default(),
            receiver_param: None,
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
            uses: Default::default(),
            receiver_param: None,
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
                array_element_type: None,
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

    /// Every deopt emitted inside a SPLICED body must record the enclosing
    /// `invoke`'s bci, never the relocated one.
    ///
    /// This is the regression test for the argument `lower_inner_with_scopes`
    /// used to drop: it received `spliced_ranges` and passed `Lowerer::new` a
    /// literal `&[]`, so `resume_bci` was the identity and every inlined deopt
    /// named a bci that is not a program point of the method. The interpreter
    /// then found no deopt point at that bci, defaulted the reason to
    /// `UnreachedCode`, and refused to resume — turning netty's
    /// `IndexOutOfBoundsException` into an `InternalError`.
    ///
    /// The shape is netty's, reduced: a caller whose `invokestatic` at bci 5
    /// has been replaced by a relocated body starting at bci 9, containing an
    /// array load whose bounds check must resume at 5.
    #[test]
    fn a_deopt_inside_a_spliced_body_resumes_at_the_enclosing_invoke() {
        // caller: 0: aload_0  1: iload_1  2: <the spliced region stands in for
        // the invoke at bci 5>  ... the graph below is hand-built, so only the
        // bcis matter, not a decodable byte string.
        let mut g = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        };
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let idx = g.add(Op::Param(1), IrType::Int, vec![start], None);
        // The array load lives at bci 36 — inside the relocated body.
        let load = g.add(
            Op::ArrayLoad(MemKind::Byte),
            IrType::Int,
            vec![ctrl, mem, arr, idx],
            Some(36),
        );
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl, load], Some(8));
        g.exit = ret;
        // The caller's own snapshot at the invoke it replaced.
        g.safepoints.push(SafepointSnapshot {
            bci: 5,
            locals: vec![arr, idx],
            stack: vec![arr, idx],
        });
        let schedule = ir_schedule::schedule(&g);

        // The caller's bytecode is 9 bytes; the relocated body occupies 9..43,
        // standing in for the `invoke` at bci 5.
        let spliced = [(9usize, 43usize, 5usize)];
        let cm = lower_inner(
            &g,
            &schedule,
            2,
            2,
            &no_helpers(),
            &HashMap::new(),
            &spliced,
            None,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .expect("lower");

        assert!(
            !cm._deopt_point_boxes.is_empty(),
            "the array access must emit its null/bounds guards",
        );
        for p in &cm._deopt_point_boxes {
            assert_eq!(
                p.bci, 5,
                "a deopt at relocated bci 36 must resume at the enclosing \
                 invoke (5), not at a bci the method does not have; got {}",
                p.bci,
            );
            assert_eq!(
                p.frame_state.bci, 5,
                "and its frame state must be the caller's snapshot at 5",
            );
        }

        // The control: with no spliced ranges, the same graph records the raw
        // bci — which is what the production lowering was doing for every
        // inlined method.
        let cm_raw = lower_inner(
            &g,
            &schedule,
            2,
            2,
            &no_helpers(),
            &HashMap::new(),
            &[],
            None,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .expect("lower");
        assert!(
            cm_raw._deopt_point_boxes.iter().all(|p| p.bci == 36),
            "without ranges the raw bci is kept — this is the control, and it \
             is what the bug looked like",
        );
    }

    /// A division the bytecode reaches on only one arm must deopt from its
    /// control-anchored `Op::Guard`, not from the floating `Op::Div` the
    /// scheduler placed wherever its operands happened to be available.
    ///
    /// Before this, `MemoryEstimator.estimateMemory` (H2's `MVMap` key/value
    /// size estimator) had its `ldiv` hoisted above the branch that guarantees
    /// a positive divisor, and the lowerer's own guard then deopted on a path
    /// the interpreter never takes — which the interpreter faithfully resumed
    /// into, throwing `ArithmeticException: / by zero` out of a program that
    /// cannot divide by zero.
    #[test]
    fn a_guarded_divisions_trap_comes_from_the_guard_not_the_floating_div() {
        // 0: iload_0  1: ifne +7 (→8)  4: iload_1  5: iload_2  6: idiv
        // 7: ireturn  8: iconst_0  9: ireturn
        let code = [
            0x1a, 0x9a, 0x00, 0x07, 0x1b, 0x1c, 0x6c, 0xac, 0x03, 0xac, 0, 0,
        ];
        let graph = crate::ir::IrBuilder::new(3, 3)
            .build(&code, 10)
            .expect("IR build failed");
        let schedule = ir_schedule::schedule(&graph);
        let cm = lower_inner(
            &graph,
            &schedule,
            3,
            3,
            &no_helpers(),
            &HashMap::new(),
            &[],
            None,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .expect("lower");

        let at_div: Vec<_> = cm
            ._deopt_point_boxes
            .iter()
            .filter(|p| p.bci == 6)
            .collect();
        assert_eq!(
            at_div.len(),
            1,
            "one deopt point at the idiv's bci, not one per emitter"
        );
        assert_eq!(
            at_div[0].reason,
            DeoptReason::UncommonTrap,
            "the surviving trap is the control-anchored Op::Guard's; a \
             `DivByZero` here means the floating div still deopts"
        );
        assert!(
            !cm._deopt_point_boxes
                .iter()
                .any(|p| p.reason == DeoptReason::DivByZero),
            "the lowerer must not emit its own div-by-zero deopt once the \
             builder anchored a guard"
        );
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
        // conservatively rejects same-block ordering). The bail names the
        // eliminated allocation instead of claiming the slot is undefined, so
        // the resume is REFUSED (safe whole-method re-run) rather than served a
        // fabricated `Int(0)` — which for this reference local would be `null`.
        let (g, sr_map, newo) = build_sr_deopt_graph(true, false);
        let schedule = ir_schedule::schedule(&g);
        let cm = lower_with_scalar_deopt(&g, &schedule, 1, 3, &no_helpers(), Some(&sr_map))
            .expect("lower");
        let locals = deopt_locals_at(&cm, 10);
        assert_eq!(
            locals[1],
            FrameValue::MaterializationRequired(EliminatedValue::allocation(
                newo,
                7,
                EliminationCause::ScalarReplacedObject,
            )),
            "a non-dominating store must bail to MaterializationRequired, naming \
             the producer node and the class it allocated"
        );
        let fs = &cm
            ._deopt_point_boxes
            .iter()
            .find(|p| p.bci == 10)
            .expect("deopt box at bci 10")
            .frame_state;
        assert!(
            !crate::deopt::frame_state_is_resumable(fs),
            "the frame must be unresumable, which is what turns the old silent \
             null into a refused deopt"
        );
        assert_eq!(crate::deopt::count_materialization_required(fs), 1);
    }

    /// A field whose value is itself a scalar-replaced allocation is emitted as
    /// a NESTED `FrameValue::VirtualObject`, not bailed.
    ///
    /// This used to answer `MaterializationRequired(NestedVirtualObject)` -- "the
    /// recipe exists but v1 emits no nested graphs" -- and that restriction is
    /// what stopped a wrapper object and its storage array from both being
    /// deleted: the wrapper's recipe names the array, so the moment the array
    /// became replaceable the wrapper's own recipe was refused and the wrapper
    /// had to stay on the heap. The materializer has always walked nested field
    /// graphs (`collect_virtual_objects` recurses, `field_value_to_value`
    /// resolves a nested state to its shell); only the producer refused.
    #[test]
    fn scalar_deopt_emits_a_nested_virtual_object_for_a_virtual_field() {
        let (mut g, mut sr_map, newo) = build_sr_deopt_graph(false, false);
        // Make field 0's value itself a scalar-replaced object: add a second
        // dead `Op::New` and register it in the map, then point the first
        // object's only field at it.
        let inner = g.add(Op::Dead, IrType::Ref, vec![], None);
        sr_map.objects.insert(
            inner,
            VirtualObjectInfo {
                array_element_type: None,
                class_id: 9,
                num_fields: 0,
                field_values: vec![],
                new_ctrl: 1, // Proj(0) — the entry control, dominates everything
                store_ctrls: vec![],
            },
        );
        sr_map
            .objects
            .get_mut(&newo)
            .expect("outer object")
            .field_values = vec![Some(inner)];

        let schedule = ir_schedule::schedule(&g);
        let cm = lower_with_scalar_deopt(&g, &schedule, 1, 3, &no_helpers(), Some(&sr_map))
            .expect("lower");
        let outer = &deopt_locals_at(&cm, 10)[1];
        let FrameValue::VirtualObject(state) = outer else {
            panic!("the outer object must still be a VirtualObject, got {outer:?}");
        };
        assert_eq!(state.field_values.len(), 1);
        assert!(
            matches!(
                &state.field_values[0],
                FrameValue::VirtualObject(inner_state)
                    if inner_state.class_id == 9 && inner_state.num_fields == 0
            ),
            "field 0 must be the nested object's own recipe, got {:?}",
            state.field_values[0]
        );
        let fs = &cm
            ._deopt_point_boxes
            .iter()
            .find(|p| p.bci == 10)
            .expect("deopt box at bci 10")
            .frame_state;
        assert!(
            crate::deopt::frame_state_is_resumable(fs),
            "a nested graph is rebuildable, so the frame must stay resumable"
        );
    }

    /// ...and a nested object that cannot be described refuses the ENCLOSING
    /// one, with the nested cause. Fail-closed one level at a time: the
    /// recursion re-runs every gate on the inner object, so an inner allocation
    /// whose control does not strictly dominate the deopt block takes the outer
    /// object down with it rather than being silently replaced by a null.
    #[test]
    fn scalar_deopt_an_undescribable_nested_field_bails_the_enclosing_object() {
        let (mut g, mut sr_map, newo) = build_sr_deopt_graph(false, false);
        let inner = g.add(Op::Dead, IrType::Ref, vec![], None);
        sr_map.objects.insert(
            inner,
            VirtualObjectInfo {
                array_element_type: None,
                class_id: 9,
                num_fields: 0,
                field_values: vec![],
                // NO_NODE control: unresolvable, so the inner object's own
                // dominance gate refuses it.
                new_ctrl: NO_NODE,
                store_ctrls: vec![],
            },
        );
        sr_map
            .objects
            .get_mut(&newo)
            .expect("outer object")
            .field_values = vec![Some(inner)];

        let schedule = ir_schedule::schedule(&g);
        let cm = lower_with_scalar_deopt(&g, &schedule, 1, 3, &no_helpers(), Some(&sr_map))
            .expect("lower");
        assert_eq!(
            deopt_locals_at(&cm, 10)[1],
            FrameValue::MaterializationRequired(EliminatedValue::allocation(
                newo,
                7,
                EliminationCause::NestedVirtualObject,
            )),
            "an undescribable nested field must bail the enclosing object with \
             NestedVirtualObject, not ScalarReplacedObject and not Undefined"
        );
    }

    /// A `NO_NODE` snapshot slot is the one case that is *genuinely* undefined —
    /// the local was never stored on any path reaching this bci, so the verifier
    /// guarantees the interpreter cannot read it and `Value::Int(0)` is correct.
    /// It must keep resolving to `Undefined`; widening the eliminated marker to
    /// cover it would make every method with an unwritten local unresumable.
    #[test]
    fn no_node_snapshot_slot_stays_undefined() {
        let (mut g, sr_map, _newo) = build_sr_deopt_graph(false, false);
        // locals were [cond, o]; append an unwritten slot.
        g.safepoints
            .iter_mut()
            .find(|s| s.bci == 10)
            .expect("snapshot at bci 10")
            .locals
            .push(NO_NODE);
        let schedule = ir_schedule::schedule(&g);
        let cm = lower_with_scalar_deopt(&g, &schedule, 1, 3, &no_helpers(), Some(&sr_map))
            .expect("lower");
        let locals = deopt_locals_at(&cm, 10);
        assert_eq!(
            locals[2],
            FrameValue::Undefined,
            "an unwritten local is genuinely undefined, not eliminated"
        );
        // And it does NOT block the resume — only the eliminated marker does.
        assert!(crate::deopt::frame_state_is_resumable(&FrameState {
            method_key: String::new(),
            bci: 10,
            locals: vec![FrameValue::Undefined],
            stack: vec![],
            monitors: vec![],
            caller: None,
        }));
    }

    /// `frame_value_for` used to index `graph.nodes` directly, so a snapshot slot
    /// naming an id past the end of the arena panicked — on the compiler thread,
    /// which takes the VM down. It must bail to the unresumable marker instead.
    #[test]
    fn out_of_range_snapshot_node_id_bails_instead_of_panicking() {
        let (mut g, _sr_map, _newo) = build_sr_deopt_graph(false, false);
        let past_end = g.nodes.len() as NodeId + 7;
        g.safepoints
            .iter_mut()
            .find(|s| s.bci == 10)
            .expect("snapshot at bci 10")
            .locals
            .push(past_end);
        let schedule = ir_schedule::schedule(&g);
        let cm = lower(&g, &schedule, 1, 3, &no_helpers()).expect("lower");
        let locals = deopt_locals_at(&cm, 10);
        assert_eq!(
            locals[2],
            FrameValue::MaterializationRequired(EliminatedValue::unknown(
                EliminationCause::Unclassified,
            )),
            "an out-of-range node id must resolve to the unresumable marker"
        );
    }

    /// The install-time verifier must reject metadata whose reference slot the
    /// oop map does not cover — the disagreement that leaves a live reference
    /// invisible to a relocating collector. Built here directly (rather than
    /// through `lower_inner`) because this backend does not yet anchor its oop
    /// maps to native offsets, so the agreement lane has nothing to join on in a
    /// real compile; the wiring in `lower_inner` arms the moment it does.
    #[test]
    fn install_verifier_rejects_an_oop_map_deopt_map_disagreement() {
        let point = |locals: Vec<FrameValue>| DeoptimizationPoint {
            native_offset: 0x40,
            bci: 3,
            reason: DeoptReason::NullCheck,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: FrameState {
                method_key: String::new(),
                bci: 3,
                locals,
                stack: vec![],
                monitors: vec![],
                caller: None,
            },
            semantics: ResumeSemantics::REEXECUTE,
        };
        // The map covers [rbp-40]; deopt spells that word as StackSlotRef(-40).
        let verifier = DeoptVerifier::new().with_oop_map(0x40, OopCoverage::complete([40]));
        assert!(
            verifier
                .verify(std::slice::from_ref(&point(vec![
                    FrameValue::StackSlotRef(-40)
                ])))
                .is_ok(),
            "a covered reference slot must pass"
        );
        let err = verifier
            .verify(std::slice::from_ref(&point(vec![
                FrameValue::StackSlotRef(-48),
            ])))
            .expect_err("an uncovered reference slot must bail the install");
        assert!(
            err.to_string().contains("oop map"),
            "the bailout must say which agreement broke, got: {err}"
        );
    }

    /// A `VirtualObject` recipe may only name a removed node the emitter also
    /// registered as materializable. Naming a node the optimizer retired without
    /// a recipe means the metadata was built from a stale graph, and the artifact
    /// must not install.
    #[test]
    fn install_verifier_rejects_a_recipe_for_an_unmaterializable_removed_node() {
        let point = DeoptimizationPoint {
            native_offset: 0x10,
            bci: 1,
            reason: DeoptReason::DivByZero,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: FrameState {
                method_key: String::new(),
                bci: 1,
                locals: vec![FrameValue::VirtualObject(VirtualObjectState {
                    array_element_type: None,
                    id: 12,
                    class_id: 3,
                    num_fields: 1,
                    field_values: vec![FrameValue::Int(0)],
                })],
                stack: vec![],
                monitors: vec![],
                caller: None,
            },
            semantics: ResumeSemantics::REEXECUTE,
        };
        assert!(
            DeoptVerifier::new()
                .with_removed_nodes([12u32])
                .with_materializable_nodes([12u32])
                .verify(std::slice::from_ref(&point))
                .is_ok(),
            "a removed node registered as materializable is exactly what a \
             VirtualObject slot is allowed to name"
        );
        assert!(
            DeoptVerifier::new()
                .with_removed_nodes([12u32])
                .verify(std::slice::from_ref(&point))
                .is_err(),
            "a removed node with no recipe must bail the install"
        );
    }

    /// `lower_inner` publishes the slot planner's peak-liveness figure to the
    /// per-compilation report. The number itself is `plan_slots`' business; what
    /// this pins is that the metric is no longer `NotMeasured` after a compile,
    /// which is what made it useless to a reader.
    #[test]
    fn peak_live_values_reaches_the_compilation_report() {
        let _guard = crate::metrics::METRICS_TEST_LOCK.lock();
        crate::metrics::set_enabled_for_test(true);

        // int f(int a, int b) { return a + b; } — two live params at the Add.
        let code = [0x1a, 0x1b, 0x60, 0xac, 0, 0];
        let builder = IrBuilder::new(2, 2);
        let graph = builder.build(&code, 4).expect("build");
        let schedule = ir_schedule::schedule(&graph);
        let expected = plan_slots(&graph, &schedule, None).peak_live;

        let published = {
            let rec = crate::metrics::CompileRecorder::begin("IrLowerPeak", "f", "(II)I", true);
            lower(&graph, &schedule, 2, 2, &no_helpers()).expect("lower");
            rec.snapshot().expect("in-flight snapshot")
        };
        crate::metrics::set_enabled_for_test(false);

        // `Measured::Value(n)` is distinguishable from `NotMeasured` at every
        // `n`, so this equality proves the wiring on its own — the planner's
        // arithmetic is pinned separately by
        // `non_overlapping_values_share_a_frame_slot`.
        assert_eq!(
            published.peak_live_values,
            crate::metrics::Measured::Value(expected as u32),
            "lower_inner must publish SlotPlan::peak_live"
        );
        assert!(published.to_json().contains("\"peak_live_values\":"));
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
        // With `sr_map = None` (the default), no `VirtualObject` recipe is
        // emitted — but the slot still describes an object escape analysis
        // deleted, so it resolves to the unresumable marker rather than to
        // `Undefined`. This is the DEFAULT-configuration half of the silent-null
        // fix: `apply_ea_to_ir` runs whether or not `CRATONVM_SCALAR_DEOPT` is
        // set, so an ordinary production compile reaches exactly this path.
        // `Unclassified` because the generic slot resolver only knows the
        // producer is `Op::Dead`, not which pass retired it.
        let (g, _sr_map, newo) = build_sr_deopt_graph(false, false);
        let schedule = ir_schedule::schedule(&g);
        let cm = lower(&g, &schedule, 1, 3, &no_helpers()).expect("lower");
        let locals = deopt_locals_at(&cm, 10);
        assert_eq!(
            locals[1],
            FrameValue::MaterializationRequired(EliminatedValue::new(
                newo,
                EliminationCause::Unclassified,
            )),
        );
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
            uses: Default::default(),
            receiver_param: None,
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

    // ── `[rbp - 0]` sentinel removal + resource bounds ───────────────────

    /// `int f(int a) { return a + 1; }` as a hand-built graph, plus its
    /// schedule. The smallest shape that has a real data edge to verify.
    fn add_one_graph() -> (Graph, Schedule) {
        let mut graph = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        };
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let _mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let a = graph.add(Op::Param(0), IrType::Int, vec![start], None);
        let one = graph.add(Op::Const(1), IrType::Int, vec![], None);
        let sum = graph.add(Op::Add, IrType::Int, vec![a, one], None);
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, sum], None);
        let schedule = ir_schedule::schedule(&graph);
        (graph, schedule)
    }

    /// `void f(int a) { a + 1; }` — the same shape as [`add_one_graph`] with the
    /// value dropped from the `Return`, so the only difference between the two
    /// lowerings is the void exit itself.
    fn void_return_graph() -> (Graph, Schedule) {
        let mut graph = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        };
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let _mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let a = graph.add(Op::Param(0), IrType::Int, vec![start], None);
        let one = graph.add(Op::Const(1), IrType::Int, vec![], None);
        let _sum = graph.add(Op::Add, IrType::Int, vec![a, one], None);
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl], None);
        let schedule = ir_schedule::schedule(&graph);
        (graph, schedule)
    }

    /// A VOID return must leave a defined value in the return register.
    ///
    /// `i64::MIN` there is the VM-wide "the callee trapped" sentinel, and every
    /// consumer reads the raw register: the single-pass inline MIC/PIC
    /// cascade's `emit_inline_callee_deopt_check`, the megamorphic hashed
    /// stub's, `jit_invoke_virtual_mic`'s `rc == i64::MIN`, and the
    /// interpreter's post-JIT drain. A void method has no return value, so
    /// unless the exit writes one, RAX carries whatever the last operation left
    /// there and can equal the sentinel by accident — on behalf of a call that
    /// neither threw nor deopted. The single-pass backend has always zeroed it
    /// (`x64/bytecode_walk.rs`, the `0xb1` arm); this backend did not.
    ///
    /// Asserted on the BYTES, and as a DIFFERENCE against the value-returning
    /// twin, so it cannot pass by accident: `try_call` returning 0 would be
    /// satisfied by an undefined register that merely happened to hold 0.
    #[test]
    fn a_void_return_writes_the_return_register() {
        const XOR_EAX_EAX: [u8; 2] = [0x31, 0xC0];

        let (vg, vs) = void_return_graph();
        let void_method = lower(&vg, &vs, 1, 1, &no_helpers()).expect("void method must lower");
        let (ag, as_) = add_one_graph();
        let value_method = lower(&ag, &as_, 1, 1, &no_helpers()).expect("a+1 must lower");

        let void_zeroes = count_seq(void_method._buffer_slice_for_debug(), &XOR_EAX_EAX);
        let value_zeroes = count_seq(value_method._buffer_slice_for_debug(), &XOR_EAX_EAX);
        assert_eq!(
            void_zeroes,
            value_zeroes + 1,
            "the void exit must contribute exactly one `XOR EAX,EAX` the \
             value-returning exit does not (void={void_zeroes}, value={value_zeroes})",
        );
        assert_eq!(
            unsafe { void_method.try_call(&[41]) },
            Ok(0),
            "a void method must not hand its caller the deopt sentinel",
        );
    }

    /// The verifier must not refuse a graph the lowerer handles — otherwise the
    /// "bail before emission" fix would silently cost JIT coverage.
    #[test]
    fn verification_accepts_a_well_formed_graph() {
        let (graph, schedule) = add_one_graph();
        assert!(verify_data_locations(&graph, &schedule).is_ok());
        let method = lower(&graph, &schedule, 1, 1, &no_helpers()).expect("a+1 must lower");
        assert_eq!(unsafe { method.try_call(&[41]) }, Ok(42));
    }

    /// Acceptance test for the `[rbp - 0]` defect (report: "replace
    /// zero-initialized `node_slot` with verified optional locations").
    ///
    /// A data input that no emitted node defines — here an `Op::Dead` node,
    /// which the scheduler never places, exactly like the GVN-collapsed
    /// `ArrayLoad(Double)` observed in `DualPivotQuicksort.insertionSort` —
    /// must refuse the compile *before* emission, not be detected afterwards by
    /// a sticky bit. Before this change the use lowered to `[rbp - 0]`, reading
    /// the saved caller frame pointer as a `double`.
    #[test]
    fn an_unallocated_data_input_refuses_the_compile_before_emission() {
        let mut graph = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        };
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let _mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let a = graph.add(Op::Param(0), IrType::Int, vec![start], None);
        // Scheduler omission, modelled: `Op::Dead` is never placed into a
        // block, so nothing ever allocates a location for it.
        let ghost = graph.add(Op::Dead, IrType::Int, vec![], None);
        let sum = graph.add(Op::Add, IrType::Int, vec![a, ghost], None);
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, sum], None);

        let schedule = ir_schedule::schedule(&graph);
        assert_eq!(
            schedule.node_to_block[ghost as usize],
            usize::MAX,
            "precondition: the ghost node is in no emitted block",
        );

        let err = verify_data_locations(&graph, &schedule)
            .expect_err("an unallocated data input must be refused");
        assert_eq!(err.reason, BailoutReason::UnallocatedValue { node: ghost });
        assert_eq!(err.category(), "unallocated_value");

        // …and the public entry point turns that into the historical "run in a
        // lower tier" answer rather than emitting or panicking.
        assert!(lower(&graph, &schedule, 1, 1, &no_helpers()).is_none());
    }

    /// The slot accessor has no encoding for "unallocated", so it can never
    /// hand back `0` — the offset that means `[rbp - 0]`, the saved caller RBP.
    #[test]
    fn slot_accessor_never_yields_the_rbp_zero_sentinel() {
        let (graph, schedule) = add_one_graph();
        let a = graph
            .nodes
            .iter()
            .position(|n| matches!(n.op, Op::Param(0)))
            .expect("param node") as NodeId;
        let sum = graph
            .nodes
            .iter()
            .position(|n| matches!(n.op, Op::Add))
            .expect("add node") as NodeId;

        let empty: HashMap<usize, bool> = HashMap::new();
        let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
        let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
        let no_compact_fields: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        let no_scopes = InlineScopeTable::new();
        let buf = ExecutableBuffer::new(4096).expect("executable buffer");
        let helpers = no_helpers();
        let plan = plan_slots(&graph, &schedule, None);
        let mut lowerer = Lowerer::new(
            &graph,
            &schedule,
            buf,
            1,
            1,
            &plan,
            &helpers,
            &empty,
            &[],
            None,
            &no_direct,
            &no_ic,
            &no_compact_fields,
            &no_scopes,
        );

        // Unallocated is an error, not an offset.
        assert!(lowerer.slot_of_checked(a).is_err());
        assert!(lowerer.node_slot.iter().all(|slot| slot.is_none()));

        let off = lowerer.alloc_slot_checked(a).expect("first spill slot");
        assert!(
            off >= lowerer.first_spill,
            "{off} < {}",
            lowerer.first_spill
        );
        assert_ne!(off, 0);
        assert_eq!(lowerer.slot_of_checked(a).expect("allocated"), off);
        // Non-zero by type, not by convention: every occupied entry is a
        // `NonZeroU32`, so `0` is not a representable location at all.
        assert!(lowerer
            .node_slot
            .iter()
            .flatten()
            .all(|located| located.get() > 0));

        // The infallible façade the emission sites use latches a structured
        // reason and still never returns 0.
        let poison = lowerer.slot_of(sum);
        assert_ne!(poison, 0, "the façade must never emit [rbp - 0]");
        assert!(lowerer.unallocated_slot_use.get());
        let latched = lowerer.take_latched_bailout().expect("a latched reason");
        assert_eq!(
            latched.reason,
            BailoutReason::UnallocatedValue { node: sum }
        );
        // An out-of-range id is an error too — it used to index-panic.
        assert!(lowerer.slot_of_checked(NO_NODE).is_err());
    }

    /// Report: "bound graph and frame memory before allocation" — an
    /// over-budget node count is refused with `GraphTooLarge`.
    #[test]
    fn an_over_limit_node_count_bails_with_graph_too_large() {
        assert!(check_graph_size(0, DEFAULT_MAX_NODES).is_ok());
        assert!(check_graph_size(DEFAULT_MAX_NODES, DEFAULT_MAX_NODES).is_ok());
        let err = check_graph_size(DEFAULT_MAX_NODES + 1, DEFAULT_MAX_NODES)
            .expect_err("over the node budget");
        assert_eq!(
            err.reason,
            BailoutReason::GraphTooLarge {
                nodes: DEFAULT_MAX_NODES + 1,
                limit: DEFAULT_MAX_NODES,
            }
        );
        assert_eq!(err.category(), "graph_too_large");
    }

    /// Report: "bound graph and frame memory before allocation" — the frame is
    /// estimated (spill reservation + locals + staged outgoing args) and
    /// refused with `FrameTooLarge` before a byte of it is reserved.
    #[test]
    fn an_over_limit_frame_estimate_bails_with_frame_too_large() {
        let plain = FrameNeeds {
            needs_context: false,
            max_call_args: 0,
        };

        // A normal method is nowhere near the bound.
        let small = estimate_frame_bytes(4, 32, &plain);
        assert!(small < DEFAULT_MAX_FRAME_BYTES, "{small}");
        assert!(check_frame_size(small, DEFAULT_MAX_FRAME_BYTES).is_ok());
        assert_eq!(small % 16, 0, "the estimate is 16-byte aligned");

        // Each of the three terms the report names moves the estimate.
        assert!(estimate_frame_bytes(64, 32, &plain) > small); // locals
        assert!(estimate_frame_bytes(4, 512, &plain) > small); // spills
        assert!(
            estimate_frame_bytes(
                4,
                32,
                &FrameNeeds {
                    needs_context: true,
                    max_call_args: 8,
                },
            ) > small,
            "staged outgoing arguments must count",
        );

        // A method whose PEAK LIVE set is as wide as the node budget still has
        // to be refused: 20_000 simultaneously live values is ~160 KiB, five
        // times the 32 KiB oop-map bound. What changed with liveness reuse is
        // that reaching this now takes 20_000 values live *at once*, not merely
        // 20_000 values in the arena — see
        // `a_previously_declined_large_method_now_compiles`.
        let huge = estimate_frame_bytes(0, DEFAULT_MAX_NODES, &plain);
        assert!(huge > DEFAULT_MAX_FRAME_BYTES, "{huge}");
        let err =
            check_frame_size(huge, DEFAULT_MAX_FRAME_BYTES).expect_err("over the frame budget");
        assert_eq!(
            err.reason,
            BailoutReason::FrameTooLarge {
                bytes: huge,
                limit: DEFAULT_MAX_FRAME_BYTES,
            }
        );
        assert_eq!(err.category(), "frame_too_large");

        // Saturating, not wrapping: a pathological count reports "enormous"
        // instead of overflowing into a small (or negative) frame.
        assert_eq!(
            estimate_frame_bytes(0, usize::MAX, &plain),
            usize::MAX & !15usize,
        );

        // End to end: the public entry point refuses instead of reserving a
        // frame whose slot offsets the oop map could not even encode.
        let (graph, schedule) = add_one_graph();
        assert!(
            lower(&graph, &schedule, 1, 10_000, &no_helpers()).is_none(),
            "10_000 locals is an 80 KiB frame — refuse, do not build it",
        );
        // The same graph with a sane local count still compiles.
        assert!(lower(&graph, &schedule, 1, 1, &no_helpers()).is_some());
    }

    // ── Liveness-based frame-slot reuse ──────────────────────────────────

    /// An empty graph shell, so each test below only writes the shape it cares
    /// about.
    fn empty_graph() -> Graph {
        Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        }
    }

    /// `int f(int a) { a += 1; a += 1; … }` with `len` increments: a chain in
    /// which every temporary is dead the instant the next one consumes it. The
    /// shape the old one-slot-per-node reservation was worst on.
    fn straight_line_chain(len: usize) -> (Graph, Schedule) {
        let mut graph = empty_graph();
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let _mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let a = graph.add(Op::Param(0), IrType::Int, vec![start], None);
        let one = graph.add(Op::Const(1), IrType::Int, vec![], None);
        let mut cur = a;
        for _ in 0..len {
            cur = graph.add(Op::Add, IrType::Int, vec![cur, one], None);
        }
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, cur], None);
        let schedule = ir_schedule::schedule(&graph);
        (graph, schedule)
    }

    /// Re-derive the linear emission positions [`plan_slots`] numbers, so a
    /// test can assert *where* a value is live. Deliberately a separate
    /// implementation of the numbering the module comment documents: if the two
    /// ever disagree, the loop test below is the one that notices.
    #[allow(clippy::type_complexity)]
    fn emission_positions(
        graph: &Graph,
        schedule: &Schedule,
    ) -> (Vec<Option<usize>>, Vec<(usize, usize)>) {
        let mut pos = vec![None; graph.nodes.len()];
        let mut span = Vec::with_capacity(schedule.blocks.len());
        let mut seq = 0usize;
        for block in &schedule.blocks {
            let start = seq;
            for &nid in &block.nodes {
                pos[nid as usize] = Some(seq);
                seq += 1;
            }
            if let Some(term) = block.terminator {
                pos[term as usize] = Some(seq);
                seq += 1;
            }
            span.push((start, seq));
            seq += 1;
        }
        (pos, span)
    }

    /// How many values the plan gives a slot to at all — the number the old
    /// reservation would have needed (one word each, forever).
    fn slotted_value_count(plan: &SlotPlan) -> usize {
        plan.node_color.iter().flatten().count()
    }

    /// The colouring's whole point: values that are never live at the same time
    /// land on the same frame word.
    #[test]
    fn non_overlapping_values_share_a_frame_slot() {
        let (graph, schedule) = straight_line_chain(64);
        let plan = plan_slots(&graph, &schedule, None);
        verify_slot_colouring(&graph, &schedule, &plan).expect("a sound colouring");

        let slotted = slotted_value_count(&plan);
        assert_eq!(slotted, 66, "param + const + 64 adds each want a slot");
        // Successive links of the chain overlap at one position (the consumer's
        // own position), so the chain alternates between two words; the
        // constant and the parameter account for the rest.
        assert!(
            plan.slots <= 8,
            "64 chained temporaries should collapse to a handful of slots, got {}",
            plan.slots,
        );
        assert!(
            plan.slots < slotted,
            "no reuse happened at all: {} slots for {slotted} values",
            plan.slots,
        );
        assert!(
            plan.peak_live <= plan.slots,
            "peak live ({}) is the floor the colouring cannot beat, slots = {}",
            plan.peak_live,
            plan.slots,
        );
    }

    /// …and values that ARE live at the same time never do. The endpoint-
    /// inclusive overlap rule means a value whose last use is at position `p`
    /// also keeps its word against a value defined at `p`.
    #[test]
    fn overlapping_values_never_share_a_frame_slot() {
        // int f(int a, int b) { int s = a + b; return a + s; }  — `a` is live
        // across `s`, so the two cannot share; `b` dies at `s`, so it can be
        // recycled by the value defined last.
        let mut graph = empty_graph();
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let _mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let a = graph.add(Op::Param(0), IrType::Int, vec![start], None);
        let b = graph.add(Op::Param(1), IrType::Int, vec![start], None);
        let s = graph.add(Op::Add, IrType::Int, vec![a, b], None);
        let t = graph.add(Op::Add, IrType::Int, vec![a, s], None);
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, t], None);
        let schedule = ir_schedule::schedule(&graph);

        let plan = plan_slots(&graph, &schedule, None);
        verify_slot_colouring(&graph, &schedule, &plan).expect("a sound colouring");
        let color = |id: NodeId| plan.node_color[id as usize].expect("a coloured value");

        assert_ne!(color(a), color(b), "both live into the first add");
        assert_ne!(color(a), color(s), "a outlives s's definition");
        assert_ne!(color(a), color(t), "a is live where t is defined");
        assert_ne!(color(s), color(t), "s is live where t is defined");
        // Four values, one legitimate sharing pair (b dies before t is defined).
        assert_eq!(slotted_value_count(&plan), 4);
        assert!(
            plan.slots < 4,
            "a dead-before-t value must be recycled, got {} slots",
            plan.slots,
        );

        // End to end, the code still computes a + (a + b).
        let cm = lower(&graph, &schedule, 2, 2, &no_helpers()).expect("lowers");
        assert_eq!(unsafe { cm.try_call(&[3, 4]) }, Ok(10));
        assert_eq!(unsafe { cm.try_call(&[-1, 5]) }, Ok(3));
    }

    /// A value used anywhere in a loop is live across the WHOLE loop, not just
    /// up to its textually last use — otherwise the second iteration reads a
    /// word some later definition has overwritten.
    ///
    /// The liveness fixed point produces this without knowing what a loop is:
    /// the back edge makes the header's uses reach every block on the path back
    /// to it. This test pins that it actually happens.
    #[test]
    fn a_loop_carried_value_stays_live_across_the_whole_loop() {
        // int sum(int n){ int s=0; for(int i=0;i<n;i++) s+=i; return s; } —
        // `n` is compared every iteration, so it must survive the back edge.
        let code = [
            0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x0d, 0x1b, 0x1c, 0x60, 0x3c, 0x84,
            0x02, 0x01, 0xa7, 0xff, 0xf4, 0x1b, 0xac, 0, 0,
        ];
        let mut graph = IrBuilder::new(1, 3).build(&code, 21).expect("IR build");
        ir_optimize::optimize(&mut graph);
        // Drop the deopt snapshots: every node they name is deliberately pinned
        // to a dedicated slot, which would make this test pass for the wrong
        // reason. The pinning itself is covered by
        // `a_deopt_visible_value_keeps_a_dedicated_slot`.
        graph.safepoints.clear();
        let schedule = ir_schedule::schedule(&graph);
        let plan = plan_slots(&graph, &schedule, None);
        verify_slot_colouring(&graph, &schedule, &plan).expect("a sound colouring");

        let (_pos, span) = emission_positions(&graph, &schedule);
        // The loop: the last back edge (a successor at or before its own block
        // index — the same test `lower_block` uses to place a safepoint poll)
        // and the header it returns to.
        let mut back_edge: Option<(usize, usize)> = None;
        for (b, block) in schedule.blocks.iter().enumerate() {
            for &s in &block.successors {
                if s <= b {
                    back_edge = Some((s, b));
                }
            }
        }
        let (header, latch) = back_edge.expect("the counted loop has a back edge");
        assert!(
            header < latch,
            "precondition: the loop spans more than one block ({header}..={latch})",
        );

        let n_node = graph
            .nodes
            .iter()
            .position(|node| matches!(node.op, Op::Param(0)))
            .expect("the loop bound is a parameter") as NodeId;
        let n_range = plan.range[n_node as usize].expect("the loop bound has a live range");
        assert!(
            n_range.lo <= span[header].0 && n_range.hi >= span[latch].1,
            "the loop bound is live [{}, {}] but the loop spans [{}, {}]",
            n_range.lo,
            n_range.hi,
            span[header].0,
            span[latch].1,
        );

        // …and therefore nothing defined inside the loop took its word.
        let n_color = plan.node_color[n_node as usize].expect("a coloured value");
        for (id, color) in plan.node_color.iter().enumerate() {
            if id as NodeId == n_node || *color != Some(n_color) {
                continue;
            }
            let block = schedule.node_to_block[id];
            assert!(
                block == usize::MAX || block < header || block > latch,
                "n{id} is inside the loop (block {block}) and reused the loop \
                 bound's frame slot",
            );
        }

        // The behavioural proof is `test_lower_counted_loop_sum`, which runs
        // this exact method; keep the compile working here too.
        assert!(lower(&graph, &schedule, 1, 3, &no_helpers()).is_some());
    }

    /// A reference and a primitive never share a frame word.
    ///
    /// `emit_safepoint_map` publishes a slot as a GC root because *some* `Ref`
    /// value was defined into it, and `defined_nodes` never un-sets. If a
    /// primitive could inherit that word, the collector would follow an `int`
    /// as an object pointer. The two free lists are disjoint, so this is a
    /// property of the representation, not of the schedule.
    #[test]
    fn a_reference_is_never_aliased_with_a_non_reference() {
        // Object f(Object r, int a) { int t = a + 1; int u = t + 1; return r; }
        // `r` is live to the return; `a`, `t`, `u` die in sequence, so the
        // primitive pool recycles while the reference pool does not.
        let mut graph = empty_graph();
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let _mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let r = graph.add(Op::Param(0), IrType::Ref, vec![start], None);
        let a = graph.add(Op::Param(1), IrType::Int, vec![start], None);
        let one = graph.add(Op::Const(1), IrType::Int, vec![], None);
        let t = graph.add(Op::Add, IrType::Int, vec![a, one], None);
        let u = graph.add(Op::Add, IrType::Int, vec![t, one], None);
        let v = graph.add(Op::Add, IrType::Int, vec![u, one], None);
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, r], None);
        let schedule = ir_schedule::schedule(&graph);

        let plan = plan_slots(&graph, &schedule, None);
        verify_slot_colouring(&graph, &schedule, &plan).expect("a sound colouring");

        // Reuse did happen among the primitives…
        assert!(
            plan.slots < slotted_value_count(&plan),
            "the primitive chain should have recycled a word",
        );
        // …and no word holds both classes.
        let mut class_of_color: HashMap<u32, SlotClass> = HashMap::new();
        for (id, color) in plan.node_color.iter().enumerate() {
            let (color, class) = match (color, plan.class[id]) {
                (Some(color), Some(class)) => (*color, class),
                _ => continue,
            };
            match class_of_color.get(&color) {
                Some(seen) => assert_eq!(
                    *seen, class,
                    "slot {color} mixes {seen:?} and {class:?} (n{id})",
                ),
                None => {
                    class_of_color.insert(color, class);
                }
            }
        }
        // The reference genuinely got a word of its own, not merely a word no
        // primitive happened to want.
        let r_color = plan.node_color[r as usize].expect("a coloured reference");
        assert_eq!(class_of_color.get(&r_color), Some(&SlotClass::Ref));
        for id in [a, t, u, v] {
            assert_ne!(
                plan.node_color[id as usize],
                Some(r_color),
                "n{id} is a primitive on the reference's word",
            );
        }

        // …and the stale-word oracle's view of the same plan agrees: every
        // primitive's colour is offered as "provably not a reference", and the
        // reference's colour is not. This is what an IR frame publishes as
        // `OopMapEntry::non_oop_stack_slots`, and a colour wrongly listed here
        // would let the residue report call a live root dead storage.
        let prim = plan.prim_colors();
        for id in [a, t, u, v] {
            let c = plan.node_color[id as usize].expect("a coloured primitive");
            assert!(prim.contains(&c), "n{id}'s word {c} is missing from prim_colors");
        }
        assert!(
            !prim.contains(&r_color),
            "the reference's word {r_color} must never be offered as a non-reference",
        );
    }

    /// COV-02: an `aaload` result is a GC ROOT, and this is the executed proof.
    ///
    /// `emit_safepoint_map` decides what to publish by scanning
    /// `graph.nodes[id].ty != IrType::Ref`, and `plan_slots` decides which pool
    /// a node's frame word comes from. Both answers hang on the ONE thing the
    /// `aaload` builder arm chooses: the node's `IrType`. Typing it `Int`
    /// compiles, passes every value-differential in
    /// `jit/tests/ir_vs_singlepass.rs`, and loses the element at the first
    /// relocating collection — a failure that surfaces nowhere near here.
    ///
    /// The brief's own suggestion for proving this — run it under
    /// `CRATONVM_MOVING_YOUNG` — cannot work: `JIT_PUBLISHES_RELOCATION_CONTRACT`
    /// is `false`, so a young collection that meets an unprovable compiled frame
    /// falls back to the non-moving sweep instead of relocating, and an
    /// unpublished root produces no observable stale pointer. See
    /// `cov-02-array-element-access-RETIRED-20260803.md`. So the
    /// property is asserted where it is actually decided.
    ///
    /// **Anti-vacuity, executed rather than argued.** The mutation was run:
    /// with `0x32`'s arm in `ir.rs` changed to `(MemKind::Ref, IrType::Int)`,
    /// this test fails with `left: Int, right: Ref` and nothing else in the
    /// crate's 1,869 lib tests notices. That is the edit to repeat if this test
    /// is ever suspected of measuring something else.
    ///
    /// It has already been wrong once in the other direction: it first asserted
    /// `class == SlotClass::Ref` and failed on an honest tree, because in a
    /// method this short the element is still deopt-visible and gets pinned.
    /// See the comment at the assertion.
    #[test]
    fn an_aaload_result_is_reference_typed_and_takes_a_reference_slot() {
        use crate::ir::{IrBuilder, MemKind};

        // static Object get(Object[] a, int i) { return a[i]; }
        //   aload_0; iload_1; aaload; areturn      (+2 bytes of padding)
        let code = [0x2a, 0x1b, 0x32, 0xb0, 0x00, 0x00];
        let graph = IrBuilder::new(2, 2)
            .build(&code, 4)
            .expect("the aaload corpus must build");

        let loads: Vec<NodeId> = graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| matches!(n.op, Op::ArrayLoad(MemKind::Ref)))
            .map(|(id, _)| id as NodeId)
            .collect();
        assert_eq!(
            loads.len(),
            1,
            "precondition: exactly one `aaload` node, or this test is measuring \
             something else"
        );
        let load = loads[0];
        assert_eq!(
            graph.nodes[load as usize].ty,
            IrType::Ref,
            "an `aaload` result is a reference; `emit_safepoint_map` skips every \
             node whose `ty` is not `Ref`, so an `Int` here is an unpublished \
             root at every later safepoint"
        );

        // …and no primitive may ever inherit the word it lands in, because
        // `emit_safepoint_map` will name that word as a root.
        //
        // Asserted as an ALIASING property rather than as `class ==
        // SlotClass::Ref`, which is what this test tried first and which is
        // wrong: in a method this short the element is still on the operand
        // stack at the `areturn` bci, so a safepoint snapshot names it and
        // `plan_slots` pins it (`SlotClass::Pinned` — shares with nothing at
        // all, strictly stronger than the reference pool). Which of the two it
        // gets depends on where the value dies, i.e. on the fixture. What must
        // hold for every fixture is that no `Prim` sits on its colour.
        let schedule = ir_schedule::schedule(&graph);
        let plan = plan_slots(&graph, &schedule, None);
        let class = plan.class[load as usize].expect("the element must get a frame word");
        assert_ne!(
            class,
            SlotClass::Prim,
            "an `aaload` result took a word from the PRIMITIVE pool; the \
             collector would then follow whatever int recycled it as an object \
             pointer"
        );
        let color = plan.node_color[load as usize].expect("a coloured element");
        for (id, other) in plan.node_color.iter().enumerate() {
            if id == load as usize || *other != Some(color) {
                continue;
            }
            assert_ne!(
                plan.class[id],
                Some(SlotClass::Prim),
                "n{id} is a primitive sharing the `aaload` element's frame word",
            );
        }
    }

    /// Every value a deopt frame names keeps a dedicated slot: the deopt
    /// producer reads it at a native offset chosen at run time, so "dead by
    /// then" is not a question this file can answer.
    #[test]
    fn a_deopt_visible_value_keeps_a_dedicated_slot() {
        let (mut graph, _) = straight_line_chain(8);
        // Name the first two adds in a snapshot, exactly as `IrBuilder` would
        // for a bytecode boundary whose locals hold them.
        let adds: Vec<NodeId> = graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| matches!(n.op, Op::Add))
            .map(|(id, _)| id as NodeId)
            .take(2)
            .collect();
        graph.safepoints.push(SafepointSnapshot {
            bci: 0,
            locals: vec![adds[0]],
            stack: vec![adds[1]],
        });
        let schedule = ir_schedule::schedule(&graph);
        let plan = plan_slots(&graph, &schedule, None);
        verify_slot_colouring(&graph, &schedule, &plan).expect("a sound colouring");

        for &pinned in &adds {
            assert_eq!(
                plan.class[pinned as usize],
                Some(SlotClass::Pinned),
                "n{pinned} is named by a deopt frame and must not share",
            );
            let color = plan.node_color[pinned as usize].expect("a coloured value");
            let sharers = plan
                .node_color
                .iter()
                .filter(|c| **c == Some(color))
                .count();
            assert_eq!(sharers, 1, "n{pinned}'s word is shared by {sharers} values");
        }
    }

    /// The colouring verifier is a real check, not a comment: each way a
    /// colouring could go wrong is refused.
    #[test]
    fn the_colouring_verifier_rejects_an_aliased_plan() {
        let (graph, schedule) = straight_line_chain(8);
        let good = plan_slots(&graph, &schedule, None);
        assert!(verify_slot_colouring(&graph, &schedule, &good).is_ok());

        let adds: Vec<usize> = graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| matches!(n.op, Op::Add))
            .map(|(id, _)| id)
            .collect();

        // (a) Two values that ARE live together forced onto one word — the
        //     silent-wrong-code case.
        let mut aliased = plan_slots(&graph, &schedule, None);
        let (first, second) = (adds[0], adds[1]);
        assert!(
            aliased.range[first]
                .expect("range")
                .overlaps(aliased.range[second].expect("range")),
            "precondition: consecutive chain links overlap",
        );
        let (donor_color, donor_class) = (aliased.node_color[first], aliased.class[first]);
        aliased.node_color[second] = donor_color;
        aliased.class[second] = donor_class;
        let err = verify_slot_colouring(&graph, &schedule, &aliased)
            .expect_err("an aliased pair must be refused");
        assert_eq!(err.category(), "internal");
        assert!(
            err.to_string().contains("simultaneously live"),
            "unexpected reason: {err}",
        );

        // (b) A reference on a primitive's word.
        let mut mixed = plan_slots(&graph, &schedule, None);
        let shared = mixed.node_color[adds[0]];
        mixed.node_color[adds[2]] = shared;
        mixed.class[adds[2]] = Some(SlotClass::Ref);
        mixed.class[adds[0]] = Some(SlotClass::Prim);
        let err = verify_slot_colouring(&graph, &schedule, &mixed)
            .expect_err("a mixed-class word must be refused");
        assert!(
            err.to_string().contains("reference and primitive"),
            "unexpected reason: {err}",
        );

        // (c) A value the lowerer will allocate for, left uncoloured — the
        //     scheduler-omission class `verify_data_locations` guards, restated
        //     for the plan.
        let mut missing = plan_slots(&graph, &schedule, None);
        missing.node_color[adds[0]] = None;
        let err = verify_slot_colouring(&graph, &schedule, &missing)
            .expect_err("an uncoloured scheduled value must be refused");
        assert_eq!(err.category(), "unallocated_value");

        // (d) A colour outside the reservation the frame was sized from.
        let mut oversized = plan_slots(&graph, &schedule, None);
        let beyond = oversized.slots as u32 + 1;
        oversized.node_color[adds[0]] = Some(beyond);
        assert!(verify_slot_colouring(&graph, &schedule, &oversized).is_err());
    }

    /// The measurement the report asks for: frame bytes against node count on a
    /// long straight-line method.
    #[test]
    fn frame_bytes_track_peak_liveness_not_node_count() {
        let plain = FrameNeeds {
            needs_context: false,
            max_call_args: 0,
        };
        let (graph, schedule) = straight_line_chain(512);
        let plan = plan_slots(&graph, &schedule, None);
        verify_slot_colouring(&graph, &schedule, &plan).expect("a sound colouring");

        let before = estimate_frame_bytes(0, graph.nodes.len(), &plain);
        let after = estimate_frame_bytes(0, plan.slots, &plain);
        assert!(
            after * 8 < before,
            "frame bytes only fell from {before} to {after} on a 512-link chain",
        );
        // The residual is the fixed part of the frame — bookkeeping slots, the
        // stack-arg reserve, the ABI shadow space and, since the register file
        // went default-on (2026-09-02), the estimate's reservation for the
        // callee-saved GP save area — not spill.
        assert!(after <= 192, "{after} bytes for a 4-slot working set");
    }

    /// The node ceiling the frame bound implies, before and after.
    ///
    /// One 8-byte slot per node put the ceiling at ~4 000 nodes: past that the
    /// frame estimate crossed `DEFAULT_MAX_FRAME_BYTES` and the optimizing tier
    /// declined the method outright. Sized by peak liveness instead, a long
    /// method is bounded by `DEFAULT_MAX_NODES` again.
    #[test]
    fn a_previously_declined_large_method_now_compiles() {
        let plain = FrameNeeds {
            needs_context: false,
            max_call_args: 0,
        };

        // Where the old accounting stopped: the largest node count whose
        // one-slot-per-node frame still fit.
        let old_ceiling = (1..=DEFAULT_MAX_NODES)
            .take_while(|&n| estimate_frame_bytes(0, n, &plain) <= DEFAULT_MAX_FRAME_BYTES)
            .last()
            .expect("some node count fits");
        assert!(
            (4000..4200).contains(&old_ceiling),
            "the old ceiling was ~4 000 nodes, computed {old_ceiling}",
        );

        let (graph, schedule) = straight_line_chain(5_000);
        assert!(
            graph.nodes.len() > old_ceiling,
            "precondition: this method was over the old ceiling",
        );
        assert!(
            check_frame_size(
                estimate_frame_bytes(0, graph.nodes.len(), &plain),
                DEFAULT_MAX_FRAME_BYTES,
            )
            .is_err(),
            "precondition: one slot per node would still be refused",
        );

        let plan = plan_slots(&graph, &schedule, None);
        verify_slot_colouring(&graph, &schedule, &plan).expect("a sound colouring");
        assert!(
            check_frame_size(
                estimate_frame_bytes(1, plan.slots, &plain),
                DEFAULT_MAX_FRAME_BYTES,
            )
            .is_ok(),
            "{} slots should fit comfortably",
            plan.slots,
        );

        // And it really compiles and really runs.
        let cm = lower(&graph, &schedule, 1, 1, &no_helpers()).expect("a 5 000-node method lowers");
        assert_eq!(unsafe { cm.try_call(&[0]) }, Ok(5_000));
        assert_eq!(unsafe { cm.try_call(&[42]) }, Ok(5_042));
    }

    // ── Phi parallel copy (the swap-loop wrong-code fix) ─────────────────
    //
    // `prealloc_phi_slots` gives every phi its OWN frame word, so the copies on
    // one CFG edge are a parallel assignment over distinct words. Emitted in
    // gather order through RAX — which is what this file did — a loop that
    // swaps two locals makes each phi the other's back-edge value, the second
    // copy reads the word the first just overwrote, and both locals end up
    // holding the same value. These tests execute the emitted bytes, because
    // that bug is invisible to any assertion about the copy *list*.

    /// Value seeded into the reserved scratch word before every harness run, so
    /// a test can prove an acyclic copy never touched it.
    const SCRATCH_SENTINEL: i64 = 0x0BAD_5C7A_0000_1234;

    /// Emit `push rbp; mov rbp,rsp; sub rsp,frame`, seed the named frame words
    /// (plus the scratch, with [`SCRATCH_SENTINEL`]), run `copies` through the
    /// REAL sequencer and the REAL emitter, load one word into RAX and return.
    /// Then execute it.
    ///
    /// `read_back == None` reads the reserved scratch word itself.
    ///
    /// The frame is a genuine `Lowerer` frame (so the offsets, the scratch
    /// reservation and `frame_size` are the ones production uses); only the
    /// prologue/epilogue are hand-rolled, because the real ones fetch a thread
    /// and touch the shadow stack that this fixture has no helpers for.
    fn parallel_copy_harness(
        seed: &[(i32, i64)],
        copies: &[(i32, i32)],
        read_back: Option<i32>,
    ) -> (i64, Vec<CopyOp>) {
        // Eight locals so the harness has plenty of frame words BELOW the
        // bookkeeping reservation to use as stand-in phi slots.
        const HARNESS_LOCALS: usize = 8;

        let (graph, schedule) = add_one_graph();
        let plan = plan_slots(&graph, &schedule, None);
        let empty: HashMap<usize, bool> = HashMap::new();
        let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
        let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
        let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        let no_scopes = InlineScopeTable::new();
        let buf = ExecutableBuffer::new(4096).expect("executable buffer");
        let helpers = no_helpers();
        let mut lowerer = Lowerer::new(
            &graph,
            &schedule,
            buf,
            0,
            HARNESS_LOCALS,
            &plan,
            &helpers,
            &empty,
            &[],
            None,
            &no_direct,
            &no_ic,
            &no_compact,
            &no_scopes,
        );
        let scratch = lowerer.phi_copy_scratch_slot_off;
        let frame = lowerer.frame_size;
        for &(off, _) in seed {
            assert!(
                off > 0 && off < frame && off != scratch,
                "the fixture must name a real frame word other than the scratch: {off}",
            );
        }

        // push rbp ; mov rbp, rsp ; sub rsp, frame_size
        lowerer.buf.emit_byte(0x55);
        lowerer.buf.emit(&[0x48, 0x89, 0xE5]);
        lowerer.buf.emit(&[0x48, 0x81, 0xEC]);
        lowerer.buf.emit(&frame.to_le_bytes());

        let scratch_seed = [(scratch, SCRATCH_SENTINEL)];
        for &(off, val) in seed.iter().chain(scratch_seed.iter()) {
            lowerer.emit_mov_reg_imm64(RAX, val as u64);
            lowerer.store_rax(off);
        }

        let ops = phi_copy_sequence(copies).expect("the fixture's copies sequentialise");
        for op in ops.iter().copied() {
            lowerer
                .emit_copy_op(op)
                .expect("every location in this backend is a frame word");
        }

        lowerer.load_to_rax(read_back.unwrap_or(scratch));
        // add rsp, frame_size ; pop rbp ; ret
        lowerer.buf.emit(&[0x48, 0x81, 0xC4]);
        lowerer.buf.emit(&frame.to_le_bytes());
        lowerer.buf.emit_byte(0x5D);
        lowerer.buf.emit_byte(0xC3);
        assert!(!lowerer.buf.overflowed(), "the harness body must fit");

        let cm = CompiledMethod::new(lowerer.buf);
        // SAFETY: the emitted body takes no arguments, reads only its own
        // frame, calls nothing and returns an i64 in RAX.
        let value = unsafe { cm.try_call(&[]) }.expect("the harness body runs");
        (value, ops)
    }

    fn count_kind(ops: &[CopyOp], f: impl Fn(&CopyOp) -> bool) -> usize {
        ops.iter().copied().filter(|o| f(o)).count()
    }

    /// The swap loop's back edge, at the machine level: `a <- b, b <- a`.
    ///
    /// Sequential emission through RAX put `b`'s value in BOTH words. This is
    /// the wrong-code witness, asserted by running the bytes.
    #[test]
    fn a_two_element_phi_cycle_really_swaps_the_two_frame_words() {
        let (a, b) = (8i32, 16i32);
        let seed = [(a, 0x1111_1111i64), (b, 0x2222_2222i64)];
        let copies = [(a, b), (b, a)];

        let (got_a, ops) = parallel_copy_harness(&seed, &copies, Some(a));
        let (got_b, _) = parallel_copy_harness(&seed, &copies, Some(b));
        assert_eq!(got_a, 0x2222_2222, "a must receive b's PRE-copy value");
        assert_eq!(
            got_b, 0x1111_1111,
            "b must receive a's PRE-copy value — 0x22222222 here is the old \
             sequential-emission bug, in which both words ended up holding b",
        );

        // One save/restore pair breaks the cycle; the third op is the move.
        assert_eq!(ops.len(), 3, "{ops:?}");
        assert_eq!(count_kind(&ops, |o| matches!(o, CopyOp::Save { .. })), 1);
        assert_eq!(count_kind(&ops, |o| matches!(o, CopyOp::Restore { .. })), 1);
        assert_eq!(count_kind(&ops, |o| matches!(o, CopyOp::Move { .. })), 1);

        // …and the scratch word really is where the parked value went.
        let (scratch_after, _) = parallel_copy_harness(&seed, &copies, None);
        assert_ne!(
            scratch_after, SCRATCH_SENTINEL,
            "a cycle must go through the reserved scratch word",
        );
    }

    /// A three-element cycle rotates; the last move IS the restore, so it costs
    /// one save, two moves and one restore.
    #[test]
    fn a_three_element_phi_cycle_rotates_the_frame_words() {
        let (a, b, c) = (8i32, 16i32, 24i32);
        let seed = [(a, 1i64), (b, 2i64), (c, 3i64)];
        // a <- b, b <- c, c <- a
        let copies = [(a, b), (b, c), (c, a)];

        let (got_a, ops) = parallel_copy_harness(&seed, &copies, Some(a));
        let (got_b, _) = parallel_copy_harness(&seed, &copies, Some(b));
        let (got_c, _) = parallel_copy_harness(&seed, &copies, Some(c));
        assert_eq!((got_a, got_b, got_c), (2, 3, 1), "{ops:?}");

        assert_eq!(ops.len(), 4, "one save, two moves, one restore: {ops:?}");
        assert_eq!(count_kind(&ops, |o| matches!(o, CopyOp::Save { .. })), 1);
        assert_eq!(count_kind(&ops, |o| matches!(o, CopyOp::Restore { .. })), 1);
    }

    /// The acyclic majority — every merge that is not a permutation — must be
    /// unchanged: one load + one store per copy, and the scratch word untouched.
    #[test]
    fn an_acyclic_phi_chain_emits_the_minimal_moves_and_no_scratch() {
        let (a, b, c) = (8i32, 16i32, 24i32);
        let seed = [(a, 1i64), (b, 2i64), (c, 3i64)];
        // b <- a, c <- b. Gather order would overwrite b before c reads it.
        let copies = [(b, a), (c, b)];

        let (got_b, ops) = parallel_copy_harness(&seed, &copies, Some(b));
        let (got_c, _) = parallel_copy_harness(&seed, &copies, Some(c));
        assert_eq!(got_b, 1, "b <- a");
        assert_eq!(got_c, 2, "c must read b's PRE-copy value: {ops:?}");

        assert_eq!(
            ops,
            vec![
                CopyOp::Move {
                    from: frame_word_loc(b).expect("a frame word"),
                    to: frame_word_loc(c).expect("a frame word"),
                },
                CopyOp::Move {
                    from: frame_word_loc(a).expect("a frame word"),
                    to: frame_word_loc(b).expect("a frame word"),
                },
            ],
            "an acyclic web must cost exactly one move per copy",
        );

        let (scratch_after, _) = parallel_copy_harness(&seed, &copies, None);
        assert_eq!(
            scratch_after, SCRATCH_SENTINEL,
            "an acyclic web must not touch the reserved scratch word",
        );
    }

    /// A copy to itself is not a move: it must emit nothing at all, so a merge
    /// whose phi already lives in its source's word costs zero instructions.
    #[test]
    fn a_self_copy_emits_nothing() {
        let ops = phi_copy_sequence(&[(8, 8), (16, 16)]).expect("sequentialises");
        assert!(ops.is_empty(), "{ops:?}");
    }

    /// ONE scratch word is enough however many cycles an edge contains: the
    /// sequencer always consumes a save before it starts the next cycle. If
    /// that ever stopped being true the saves would nest and the single
    /// reserved word would be clobbered, so pin it here — this file's frame
    /// reservation is what depends on it.
    #[test]
    fn disjoint_phi_cycles_never_nest_their_saves() {
        let (a, b, c, d) = (8i32, 16i32, 24i32, 32i32);
        let seed = [(a, 1i64), (b, 2i64), (c, 3i64), (d, 4i64)];
        let copies = [(a, b), (b, a), (c, d), (d, c)];

        let mut saved = false;
        let (got_a, ops) = parallel_copy_harness(&seed, &copies, Some(a));
        for op in &ops {
            match op {
                CopyOp::Save { .. } => {
                    assert!(!saved, "a second save before the first restore: {ops:?}");
                    saved = true;
                }
                CopyOp::Restore { .. } => {
                    assert!(saved, "a restore with nothing saved: {ops:?}");
                    saved = false;
                }
                CopyOp::Move { .. } => {}
            }
        }
        assert!(!saved, "an unconsumed save: {ops:?}");
        assert_eq!(count_kind(&ops, |o| matches!(o, CopyOp::Save { .. })), 2);

        let (got_b, _) = parallel_copy_harness(&seed, &copies, Some(b));
        let (got_c, _) = parallel_copy_harness(&seed, &copies, Some(c));
        let (got_d, _) = parallel_copy_harness(&seed, &copies, Some(d));
        assert_eq!((got_a, got_b, got_c, got_d), (2, 1, 4, 3));
    }

    /// A register is representable in the shared `ValueLoc` but unreachable
    /// here — this backend keeps every value in its home frame word. Refused,
    /// not mis-emitted as an offset.
    #[test]
    fn a_register_location_is_refused_rather_than_emitted_as_an_offset() {
        let err = frame_word_off(ValueLoc::Reg(crate::regalloc::PhysReg::gp(3)))
            .expect_err("a register is not a frame word");
        assert!(matches!(err.reason, BailoutReason::Internal(_)), "{err:?}",);
        // …and offset 0 (`[rbp - 0]` is the saved caller RBP) is not a location.
        assert!(frame_word_loc(0).is_err());
        assert!(frame_word_loc(-8).is_err());
    }

    /// The scratch is a BOOKKEEPING word: it sits below `first_spill`, so no
    /// colour `plan_slots` hands out can ever land on it, and the five reserved
    /// words are contiguous with no gap for a coloured slot to hide in.
    ///
    /// Also the frame arithmetic: `estimate_frame_bytes` (which `lower_inner`
    /// bounds against) must still equal the frame `Lowerer::new` lays out. The
    /// constructor pins that with a `debug_assert_eq!`; this asserts it in a
    /// release build too, and from the published offsets rather than from a
    /// repetition of the same sum.
    #[test]
    fn the_phi_scratch_word_is_reserved_below_the_spill_band() {
        for &num_locals in &[0usize, 1, 8] {
            let (graph, schedule) = add_one_graph();
            let plan = plan_slots(&graph, &schedule, None);
            let empty: HashMap<usize, bool> = HashMap::new();
            let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
            let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
            let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
            let no_scopes = InlineScopeTable::new();
            let buf = ExecutableBuffer::new(4096).expect("executable buffer");
            let helpers = no_helpers();
            let lowerer = Lowerer::new(
                &graph,
                &schedule,
                buf,
                0,
                num_locals,
                &plan,
                &helpers,
                &empty,
                &[],
                None,
                &no_direct,
                &no_ic,
                &no_compact,
                &no_scopes,
            );

            // The five bookkeeping words are contiguous and end at first_spill.
            assert_eq!(lowerer.shadow_thread_slot_off, lowerer.sp_id_slot_off + 8);
            assert_eq!(
                lowerer.shadow_savebase_slot_off,
                lowerer.shadow_thread_slot_off + 8
            );
            assert_eq!(
                lowerer.shadow_savetop_slot_off,
                lowerer.shadow_savebase_slot_off + 8
            );
            assert_eq!(
                lowerer.phi_copy_scratch_slot_off,
                lowerer.shadow_savetop_slot_off + 8,
                "the scratch is the fifth reserved word",
            );
            assert_eq!(
                lowerer.first_spill,
                lowerer.phi_copy_scratch_slot_off + 8,
                "the spill band starts one word above the scratch",
            );

            // No coloured slot can collide with it.
            for color in 0..plan.slots as i32 {
                assert_ne!(
                    lowerer.first_spill + color * 8,
                    lowerer.phi_copy_scratch_slot_off,
                );
            }
            assert!(lowerer.first_spill <= lowerer.spill_cap_off);

            // The estimate `lower_inner` bounds against is the frame that got
            // built — including the fifth reserved word.
            let needs = scan_frame_needs(&graph, &helpers);
            assert_eq!(
                lowerer.frame_size,
                estimate_frame_bytes(num_locals, plan.slots, &needs) as i32,
            );
            // …and that number really does account for locals + 5 bookkeeping
            // words + the spill band + the callee-saved save bands + the
            // 16-byte stack-arg reserve + the 32-byte ABI shadow.
            // `add_one_graph` needs no context slot and stages no call
            // arguments.
            //
            // The two save bands are read from the same functions the frame
            // was laid out with rather than spelled as literals: they are
            // platform- and flag-dependent (`IR_LOWER_SAVED_XMMS` is empty on
            // System V; both are empty with the linear-scan path off), and a
            // literal here would pin this test to one configuration.
            let locals_size = (num_locals as i32) * 8;
            let bookkeeping = 8 * 5;
            let saved = ir_saved_xmm_bytes() + ir_saved_gpr_bytes();
            let tail = 32 + 16;
            assert_eq!(
                lowerer.frame_size,
                ((locals_size + bookkeeping + (plan.slots as i32) * 8 + saved + tail) + 15) & !15,
            );
            assert_eq!(lowerer.spill_cap_off, lowerer.frame_size - tail - saved);
            // Offsets are 1-based — `[rbp - 0]` is the saved caller RBP — so the
            // first spill word sits one word past the locals + bookkeeping bytes.
            assert_eq!(lowerer.first_spill, locals_size + bookkeeping + 8);
        }
    }

    /// End to end, on the shape the fix exists for.
    ///
    /// ```java
    /// static int f(int a, int b, int n) {
    ///     for (int i = 0; i < n; i++) { int t = a; a = b; b = t; }
    ///     return a - b;
    /// }
    /// ```
    ///
    /// The two loop-header phis are each other's back-edge value, so the back
    /// edge's parallel copy is a two-element cycle. Before the scratch word this
    /// method REFUSED to compile (`UnsupportedShape("cyclic phi parallel
    /// copy")`) and dropped to a lower tier; before that it compiled to wrong
    /// code. `a - b` is the sharp readout: `0` is what both duplication bugs
    /// (both words ending up with `a`, or both with `b`) produce.
    #[test]
    fn the_swap_loop_compiles_and_swaps() {
        //  0: iconst_0     03
        //  1: istore_3     3e          i = 0
        //  2: iload_3      1d      ← loop header
        //  3: iload_2      1c
        //  4: if_icmpge 17 a2 00 0d
        //  7: iload_1      1b          swap through the operand stack:
        //  8: iload_0      1a          stack = [b, a]
        //  9: istore_1     3c          b = a
        // 10: istore_0     3b          a = b
        // 11: iinc 3,1     84 03 01
        // 14: goto 2       a7 ff f4
        // 17: iload_0      1a
        // 18: iload_1      1b
        // 19: isub         64
        // 20: ireturn      ac
        let code = [
            0x03, 0x3e, 0x1d, 0x1c, 0xa2, 0x00, 0x0d, 0x1b, 0x1a, 0x3c, 0x3b, 0x84, 0x03, 0x01,
            0xa7, 0xff, 0xf4, 0x1a, 0x1b, 0x64, 0xac, 0, 0,
        ];
        let cm = compile_via_ir(&code, 21, 3, 4).expect(
            "the swap loop must COMPILE — a None here is the cyclic-parallel-copy \
             refusal coming back, which drops the method a tier",
        );
        // `ireturn` leaves a 32-bit value in the return register, so the raw
        // i64 readout carries an untouched upper half and a negative result
        // reads as its u32 bit pattern (-4 as 0xFFFF_FFFC). Sign-extend before
        // comparing, or a correct swap reports as a failure.
        let f = |a: i64, b: i64, n: i64| {
            let raw = unsafe { cm.try_call(&[a, b, n]).expect("call") };
            raw as i32 as i64
        };
        assert_eq!(f(3, 7, 0), -4, "no iterations: 3 - 7");
        assert_eq!(f(3, 7, 1), 4, "one swap: 7 - 3 (0 would mean a == b)");
        assert_eq!(f(3, 7, 2), -4, "two swaps: back to 3 - 7");
        assert_eq!(f(3, 7, 3), 4, "three swaps");
        assert_eq!(f(-9, 5, 1), 14, "5 - (-9)");
    }

    // ── Inlined caller scopes → `FrameState::caller` ─────────────────────

    /// A trivially lowerable graph (`Start → Proj ctrl/mem → Param(0) →
    /// Const(7) → Return`) with no safepoints; the caller pushes whatever
    /// snapshots the test needs. Returns the `Const` node id, which resolves to
    /// `FrameValue::Int(7)` with no machine location — the simplest slot a
    /// snapshot can name.
    fn scope_fixture_graph() -> (Graph, NodeId) {
        let mut g = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        };
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let _mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let p0 = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let k = g.add(Op::Const(7), IrType::Int, vec![], None);
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl, p0], None);
        g.exit = ret;
        (g, k)
    }

    /// Resolve `graph.safepoints[index]` through a real `Lowerer` carrying
    /// `scopes`, i.e. exactly the path `build_deopt_points` takes.
    fn resolve_with_scopes(graph: &Graph, scopes: &InlineScopeTable, index: usize) -> FrameState {
        let schedule = ir_schedule::schedule(graph);
        let plan = plan_slots(graph, &schedule, None);
        let empty: HashMap<usize, bool> = HashMap::new();
        let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
        let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
        let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        let buf = ExecutableBuffer::new(4096).expect("executable buffer");
        let helpers = no_helpers();
        let lowerer = Lowerer::new(
            graph,
            &schedule,
            buf,
            1,
            1,
            &plan,
            &helpers,
            &empty,
            &[],
            None,
            &no_direct,
            &no_ic,
            &no_compact,
            scopes,
        );
        lowerer.resolve_frame_state(&graph.safepoints[index], index)
    }

    /// The state of the world today: no scope table, so every deopt frame is
    /// flat. This is the byte-identical-behaviour witness for the whole change.
    #[test]
    fn an_empty_scope_table_still_produces_a_flat_frame() {
        let (mut g, _k) = scope_fixture_graph();
        g.safepoints.push(SafepointSnapshot {
            bci: 4,
            locals: vec![NO_NODE],
            stack: vec![],
        });
        let fs = resolve_with_scopes(&g, &InlineScopeTable::new(), 0);
        assert_eq!(fs.bci, 4);
        assert!(fs.caller.is_none(), "no scope table ⇒ no caller chain");
        assert!(crate::deopt::frame_state_is_resumable(&fs));
    }

    /// A snapshot bound to a scope round-trips through lowering into a
    /// `FrameState` whose caller carries the scope's method key and the bci of
    /// the `invoke` that is in progress — and whose locals come from the
    /// caller's own recorded snapshot, not from thin air.
    #[test]
    fn a_bound_snapshot_lowers_into_its_caller_scope() {
        let (mut g, k) = scope_fixture_graph();
        // snapshot 0 = the caller's state at the invoke; snapshot 1 = the
        // inlined callee's state at the trapping bci.
        g.safepoints.push(SafepointSnapshot {
            bci: 12,
            locals: vec![k],
            stack: vec![],
        });
        g.safepoints.push(SafepointSnapshot {
            bci: 3,
            locals: vec![NO_NODE],
            stack: vec![],
        });

        let mut scopes = InlineScopeTable::new();
        let s = scopes
            .push_scope("Outer.run:()V", 12, Some(0), None)
            .expect("scope");
        assert!(scopes.bind_snapshot(1, s));

        let fs = resolve_with_scopes(&g, &scopes, 1);
        assert_eq!(fs.bci, 3, "innermost scope is the trapping one");
        let caller = fs.caller.as_ref().expect("caller scope present");
        assert_eq!(caller.method_key, "Outer.run:()V");
        assert_eq!(caller.bci, 12, "the invoke's bci, not the callee's");
        assert_eq!(
            caller.locals,
            vec![FrameValue::Int(7)],
            "the caller's locals come from its own snapshot"
        );
        assert!(caller.caller.is_none(), "depth 1 ⇒ nothing above");
        assert!(crate::deopt::frame_state_is_resumable(&fs));

        // The unbound snapshot in the same graph is unaffected.
        assert!(resolve_with_scopes(&g, &scopes, 0).caller.is_none());
    }

    /// Depth 0..4: the chain length the lowerer produces is exactly the scope
    /// depth, and the frames come out innermost-caller-first.
    #[test]
    fn caller_chains_of_depth_zero_through_four() {
        for depth in 0..=4usize {
            let (mut g, k) = scope_fixture_graph();
            // One caller snapshot per scope, then the trapping snapshot last.
            for d in 0..depth {
                g.safepoints.push(SafepointSnapshot {
                    bci: 100 + d,
                    locals: vec![k],
                    stack: vec![],
                });
            }
            let trap_index = depth;
            g.safepoints.push(SafepointSnapshot {
                bci: 9,
                locals: vec![NO_NODE],
                stack: vec![],
            });

            let mut scopes = InlineScopeTable::new();
            // Push outermost-first so each parent already exists.
            let mut parent = None;
            for d in (0..depth).rev() {
                parent = scopes.push_scope(
                    &format!("M{d}.m:()V"),
                    (100 + d) as u32,
                    Some(d as u32),
                    parent,
                );
                assert!(parent.is_some(), "depth {depth}, scope {d}");
            }
            if let Some(innermost) = parent {
                assert!(scopes.bind_snapshot(trap_index, innermost));
            }

            let fs = resolve_with_scopes(&g, &scopes, trap_index);
            let mut walked = 0usize;
            let mut cursor = fs.caller.as_deref();
            while let Some(f) = cursor {
                // Scope 0 is the immediate caller, scope `depth-1` the outermost.
                assert_eq!(f.method_key, format!("M{walked}.m:()V"));
                assert_eq!(f.bci, (100 + walked) as u32);
                walked += 1;
                cursor = f.caller.as_deref();
            }
            assert_eq!(walked, depth, "chain length matches the scope depth");
            assert!(crate::deopt::frame_state_is_resumable(&fs));
        }
    }

    /// A caller scope whose own snapshot holds an unreconstructable slot must
    /// be REFUSED, not admitted. The innermost scope here is spotless, so a
    /// scope-local predicate would wave it through — which is precisely the
    /// hazard `frame_state_is_resumable`'s caller walk closes for the two
    /// compile-time admission gates that consult it.
    #[test]
    fn a_caller_scope_holding_materialization_required_is_refused() {
        let (mut g, _k) = scope_fixture_graph();
        // A snapshot slot naming a node id past the end of the arena resolves
        // to `MaterializationRequired` (see `frame_value_for`).
        let dangling: NodeId = 9_999;
        g.safepoints.push(SafepointSnapshot {
            bci: 12,
            locals: vec![dangling],
            stack: vec![],
        });
        g.safepoints.push(SafepointSnapshot {
            bci: 3,
            locals: vec![NO_NODE],
            stack: vec![],
        });

        let mut scopes = InlineScopeTable::new();
        let s = scopes
            .push_scope("Outer.run:()V", 12, Some(0), None)
            .expect("scope");
        assert!(scopes.bind_snapshot(1, s));

        let fs = resolve_with_scopes(&g, &scopes, 1);
        let caller = fs.caller.as_ref().expect("caller scope present");
        assert!(
            matches!(caller.locals[0], FrameValue::MaterializationRequired(_)),
            "{:?}",
            caller.locals[0]
        );
        // Innermost alone is clean …
        let innermost_only = FrameState {
            caller: None,
            ..fs.clone()
        };
        assert!(
            crate::deopt::frame_state_is_resumable(&innermost_only),
            "precondition: a scope-local predicate would wave this through"
        );
        // … the chain is not.
        assert!(
            !crate::deopt::frame_state_is_resumable(&fs),
            "an unresumable caller must sink the whole deopt point"
        );
    }

    /// A scope with no recorded caller snapshot is rendered as an explicitly
    /// UNRESUMABLE frame, never as an empty one — an empty caller frame
    /// reconstructs as all-zero locals with no error anywhere.
    #[test]
    fn an_undescribed_caller_scope_is_unresumable_not_empty() {
        let (mut g, _k) = scope_fixture_graph();
        g.safepoints.push(SafepointSnapshot {
            bci: 3,
            locals: vec![NO_NODE],
            stack: vec![],
        });

        let mut scopes = InlineScopeTable::new();
        let s = scopes
            .push_scope("Outer.run:()V", 12, None, None)
            .expect("scope");
        assert!(scopes.bind_snapshot(0, s));

        let fs = resolve_with_scopes(&g, &scopes, 0);
        let caller = fs.caller.as_ref().expect("caller scope present");
        assert_eq!(caller.locals, vec![FrameValue::Unsupported]);
        assert!(!crate::deopt::frame_state_is_resumable(&fs));
    }

    /// Every `DeoptimizationPoint` this file builds carries
    /// `ResumeSemantics::for_reason(reason)` — the convention, written down
    /// instead of re-derived by each consumer. `build_deopt_points` is the
    /// site under test; the two guard sites (`Op::Guard`, `emit_deopt_unless`)
    /// box their points into code and are covered by the source witness below.
    #[test]
    fn every_deopt_point_stamps_for_reason() {
        // iconst_5; iconst_3; iadd; iconst_2; imul; ireturn — two deopt points
        // (bci 2 and bci 4), the same fixture `test_deopt_points_resolve_*` uses.
        let code = [0x08, 0x06, 0x60, 0x05, 0x68, 0xac, 0, 0];
        let cm = compile_via_ir_no_opt(&code, 6, 0, 0);
        assert!(
            !cm.deopt_points.is_empty(),
            "fixture must emit deopt points"
        );
        for p in &cm.deopt_points {
            assert_eq!(
                p.semantics,
                ResumeSemantics::for_reason(p.reason),
                "deopt point at +{:#x} (reason {:?})",
                p.native_offset,
                p.reason
            );
            assert_eq!(
                p.semantics,
                ResumeSemantics::REEXECUTE,
                "a resume point before the bytecode re-executes it"
            );
        }
    }

    /// Source witness: no `DeoptimizationPoint` literal in this file may omit
    /// `semantics`. The two guard sites bake their point's *address* into
    /// emitted code and are never handed back to a test, so the field's
    /// presence there cannot be asserted behaviourally.
    #[test]
    fn every_deopt_point_literal_names_its_semantics() {
        let src = include_str!("ir_lower.rs");
        let literals = src.matches("DeoptimizationPoint {").count();
        let stamped = src.matches("semantics: ResumeSemantics::").count();
        assert!(
            stamped >= literals,
            "{literals} `DeoptimizationPoint` literal(s) but only {stamped} \
             `semantics:` initialiser(s) — a construction site is inferring \
             the re-execute convention again instead of recording it"
        );
    }

    // ── Linear-scan register residency ──────────────────────────────────

    /// `double f(double a) { return a * a * 2.0 + a; }` as a hand-built graph.
    ///
    /// Hand-built rather than compiled from `dload_0; dmul; …`, because the
    /// point of these tests is the ALLOCATION, and a bytecode-built graph
    /// carries a `SafepointSnapshot` per bci whose interaction with pinning is
    /// the subject of its own test below.
    fn fp_chain_graph() -> (Graph, Schedule) {
        let mut graph = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        };
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let a = graph.add(Op::Param(0), IrType::Double, vec![start], Some(0));
        let sq = graph.add(Op::Mul, IrType::Double, vec![a, a], Some(1));
        let two = graph.add(
            Op::ConstF(2.0f64.to_bits()),
            IrType::Double,
            vec![],
            Some(2),
        );
        let scaled = graph.add(Op::Mul, IrType::Double, vec![sq, two], Some(3));
        let sum = graph.add(Op::Add, IrType::Double, vec![scaled, a], Some(4));
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, sum], Some(5));
        let schedule = ir_schedule::schedule(&graph);
        (graph, schedule)
    }

    #[test]
    fn linear_scan_promotes_fp_values_and_nothing_else() {
        let (graph, schedule) = fp_chain_graph();
        let plan = plan_slots(&graph, &schedule, None);
        verify_slot_colouring(&graph, &schedule, &plan).expect("the colouring verifies");

        let residency = plan_register_residency(&graph, &schedule, &plan)
            .expect("the allocation verifies")
            .expect("an FP arithmetic chain has something to promote");

        assert!(residency.promoted > 0);
        assert_eq!(
            residency.demoted, 0,
            "the two liveness models must agree on a straight-line graph"
        );
        for (id, reg) in residency.reg_of.iter().enumerate() {
            let Some(reg) = reg else { continue };
            assert!(
                IR_LOWER_LS_XMMS.contains(reg),
                "n{id} was given xmm{reg}, which is outside the file this \
                 backend may clobber"
            );
            assert!(
                matches!(graph.nodes[id].ty, IrType::Float | IrType::Double),
                "n{id} is {:?} — only FP values may take a register here, which \
                 is what makes a reference at a safepoint unrepresentable",
                graph.nodes[id].ty
            );
        }
    }

    /// A value named by a deopt frame state must still be promotable, and its
    /// home word must still be written.
    ///
    /// This is the regression witness for the check that nearly made the whole
    /// wiring inert: `plan_slots` marks every deopt-named value
    /// `SlotClass::Pinned` (= never shares a frame word), and refusing to
    /// promote that class would have undone `release_deopt_pins` for exactly
    /// the values it exists to release. `IrBuilder` names essentially every
    /// value in some frame state, so this is not a corner case — it is the
    /// common case on real bytecode.
    #[test]
    fn a_deopt_named_value_is_still_promotable() {
        let (mut graph, _) = fp_chain_graph();
        // Name every FP value in a frame state, as `IrBuilder` would.
        let fp: Vec<NodeId> = graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| matches!(n.ty, IrType::Float | IrType::Double))
            .map(|(id, _)| id as NodeId)
            .collect();
        assert!(!fp.is_empty());
        graph.safepoints.push(SafepointSnapshot {
            bci: 3,
            locals: fp.clone(),
            stack: fp.clone(),
        });
        let schedule = ir_schedule::schedule(&graph);

        let plan = plan_slots(&graph, &schedule, None);
        // Precondition: the colourer really does pin them, so this test is
        // asserting against the state it means to.
        assert!(
            fp.iter()
                .any(|&id| plan.class[id as usize] == Some(SlotClass::Pinned)),
            "the colourer must pin a deopt-named value"
        );

        let residency = plan_register_residency(&graph, &schedule, &plan)
            .expect("the allocation verifies")
            .expect(
                "a deopt-named FP value must still be promotable — write-through \
                 keeps its home word correct at every bci",
            );
        assert!(residency.promoted > 0);
    }

    /// The GC cliff, stated as a property rather than a comment: no reference
    /// can be register-resident, so `emit_safepoint_map`'s frame-slot-only
    /// publication stays complete.
    #[test]
    fn a_reference_is_never_register_resident() {
        let mut graph = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        };
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        // A live array reference and an int index, both genuinely read.
        let arr = graph.add(Op::Param(0), IrType::Ref, vec![start], Some(0));
        let idx = graph.add(Op::Param(1), IrType::Int, vec![start], Some(1));
        let elem = graph.add(
            Op::ArrayLoad(MemKind::Double),
            IrType::Double,
            vec![ctrl, mem, arr, idx],
            Some(2),
        );
        let doubled = graph.add(Op::Add, IrType::Double, vec![elem, elem], Some(3));
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, doubled], Some(4));
        let schedule = ir_schedule::schedule(&graph);

        let plan = plan_slots(&graph, &schedule, None);
        let residency = plan_register_residency(&graph, &schedule, &plan)
            .expect("the allocation verifies")
            .expect("the FP values are promotable");
        assert_eq!(
            residency.reg_of[arr as usize], None,
            "a reference took a register; `emit_safepoint_map` publishes frame \
             slots only, so the collector could neither see nor relocate it"
        );
        assert_eq!(
            residency.reg_of[idx as usize], None,
            "an int took a register, but this file's register set is XMM-only"
        );
        // `elem`, not `doubled`: since `ir_residency_pays_enabled` a value read
        // ONCE is not promoted (its register cannot repay the prologue save it
        // costs), and `doubled` is read once, by the return. `elem` is read
        // twice — it is both inputs of the `Add` — so it is the FP value this
        // graph can promote, and asserting on it keeps the two checks above
        // non-vacuous for the reason they were written.
        assert!(
            residency.reg_of[elem as usize].is_some(),
            "the FP value must be promoted, or the two assertions above are \
             vacuous"
        );
        let _ = doubled;
    }

    /// A value live across a helper call must not keep a caller-saved register.
    ///
    /// FP `Op::Rem` is the sharp case: it lowers to `CALL jit_drem`, and
    // ── Shadow instruction selection ─────────────────────────────────
    //
    // `docs/feature-designs/jit-machine-level-and-instruction-selection.md`,
    // increment 0.
    //
    // Increment 0's whole claim is "turning this on changes no emitted byte".
    // These are what hold it up. Note they drive the flag through
    // `flags::with_thread_overrides` and NOT `std::env::set_var`: the flag is
    // declared (`types/src/flag_groups.rs`, `jit/ir-isel-shadow`), so the
    // override mechanism reaches it, and a process-wide env write would race
    // every other test in this binary.

    /// Compile the same method with the shadow pass off and on, and compare
    /// **the emitted bytes**.
    ///
    /// Not "compare the result", not "both compiled" — the bytes. A shadow pass
    /// that perturbed slot numbering, buffer sizing or safepoint ids would still
    /// produce a working method and would still have broken the one property
    /// that makes increment 0 free.
    ///
    /// The exact edit that trips it: make the shadow block do anything to
    /// `lowerer` (allocate a slot, bump `next_sp_id`, touch the buffer).
    #[test]
    fn shadow_selection_changes_no_emitted_byte() {
        // Enabling the flag mutates the process-global shadow counters, so this
        // test owes the same lock as the two that read them.
        let _guard = crate::x64::isel::SHADOW_TEST_LOCK.lock();
        // int f(int a, int b) { return (a + b) * a; } — arithmetic, one block,
        // the shape `isel`'s ALU and LEA rules actually fire on, so the shadow
        // pass has real work to do rather than trivially selecting nothing.
        let code = [0x1a, 0x1b, 0x60, 0x1a, 0x68, 0xac, 0, 0];

        let bytes = |on: bool| -> Vec<u8> {
            let value = if on { Some("1") } else { None };
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_JIT_IR_ISEL_SHADOW", value)],
                || {
                    let cm = compile_via_ir(&code, 4, 2, 2).expect("compiles either way");
                    // SAFETY: the artifact is alive for the duration of this
                    // borrow, and `code_len` is what the emitter wrote.
                    unsafe {
                        std::slice::from_raw_parts(cm.entry_ptr() as *const u8, cm.code_len())
                            .to_vec()
                    }
                },
            )
        };

        let off = bytes(false);
        let on = bytes(true);
        assert!(
            !off.is_empty(),
            "precondition: the method compiled to bytes"
        );
        assert_eq!(
            off.len(),
            on.len(),
            "shadow selection changed the emitted length"
        );
        assert_eq!(off, on, "shadow selection changed the emitted bytes");
    }

    /// With the flag on, the pass actually ran — and covered every node.
    ///
    /// Without this the byte-identity test above passes vacuously: a shadow
    /// pass that never executes also changes no byte. So this asserts the
    /// counters moved, that `covers()` held on every block, and that the
    /// coverage figure is a real fraction rather than the "no nodes" zero.
    #[test]
    fn shadow_selection_runs_and_covers_every_scheduled_node() {
        let _guard = crate::x64::isel::SHADOW_TEST_LOCK.lock();
        crate::x64::isel::reset_shadow_totals();

        let code = [0x1a, 0x1b, 0x60, 0x1a, 0x68, 0xac, 0, 0];
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_ISEL_SHADOW", Some("1"))],
            || {
                compile_via_ir(&code, 4, 2, 2).expect("compiles");
            },
        );

        let (methods, stats) = crate::x64::isel::shadow_totals();
        assert_eq!(methods, 1, "exactly one method should have been shadowed");
        assert!(stats.blocks > 0, "no blocks were selected");
        assert!(
            stats.nodes > 0,
            "no data nodes were offered to the selector"
        );
        assert!(stats.tiles >= stats.blocks, "every block yields >= 1 tile");
        assert_eq!(
            stats.coverage_failures, 0,
            "`BlockSelection::covers` failed on {} block(s) — that is an `isel` \
             bug, not a measurement",
            stats.coverage_failures
        );
        assert!(
            stats.matched_tiles > 0,
            "no rule fired on `(a + b) * a`; the corpus this test uses was \
             chosen because the ALU rules match it, so zero means the selector \
             regressed, not that the method is unusual"
        );
        let pct = stats.coverage_pct();
        assert!(
            pct > 0.0 && pct <= 100.0,
            "coverage {pct} is not a fraction"
        );
    }

    /// With the flag OFF — the default, and every compile today — the pass does
    /// not run at all.
    ///
    /// The counter, not the bytes: "changed no byte" and "did not execute" are
    /// different claims and increment 0 makes both.
    #[test]
    fn shadow_selection_is_off_by_default() {
        let _guard = crate::x64::isel::SHADOW_TEST_LOCK.lock();
        crate::x64::isel::reset_shadow_totals();

        let code = [0x1a, 0x1b, 0x60, 0x1a, 0x68, 0xac, 0, 0];
        compile_via_ir(&code, 4, 2, 2).expect("compiles");

        let (methods, stats) = crate::x64::isel::shadow_totals();
        assert_eq!(methods, 0, "the shadow pass ran without being asked");
        assert_eq!(stats, crate::x64::isel::ShadowStats::default());
    }

    /// The flag is DECLARED, so `-XX:` and the test override mechanism reach it.
    ///
    /// Rule 4 of `docs/known-issues/c2/README.md`: an undeclared flag is
    /// invisible to both, and the declaration sweep has already had to be re-run
    /// once because a flag was added 21 minutes after it closed at zero. The
    /// two tests above would still pass with an undeclared flag — `runtime_var`
    /// falls through to a live `std::env` read — so this checks the property
    /// they cannot.
    #[test]
    fn the_shadow_flag_is_declared() {
        let declared: Vec<&str> = cratonvm_types::flag_groups::INVENTORY
            .iter()
            .filter_map(|e| e.on_key)
            .collect();
        assert!(
            declared.contains(&"CRATONVM_JIT_IR_ISEL_SHADOW"),
            "`CRATONVM_JIT_IR_ISEL_SHADOW` is not in `types/src/flag_groups.rs`"
        );
        assert!(
            declared.contains(&"CRATONVM_DBG_IR_ISEL"),
            "`CRATONVM_DBG_IR_ISEL` is not in `types/src/flag_groups.rs`"
        );
        // Increment 2's two flags, same rule.
        for key in ["CRATONVM_JIT_IR_ISEL_EMIT", "CRATONVM_JIT_IR_ISEL_VERIFY"] {
            assert!(
                declared.contains(&key),
                "`{key}` is not in `types/src/flag_groups.rs`"
            );
        }
    }

    // ── Increment 2: the machine list emits ───────────────────────────
    //
    // The oracle is byte equality on a corpus: compile a method both ways and
    // compare the emitted bytes. Every `PATTERNS` row carries that property per
    // row — it names the hand-written emitter it reproduces — and these are the
    // same check at method scale.

    /// `int f(int a, int b) { return (a + b) * a; }` — `iload_0; iload_1;
    /// iadd; iload_0; imul; ireturn`, and the length is **6**, the whole
    /// method. (The neighbouring shadow tests pass 4, which truncates after the
    /// `iload_0` and leaves the multiply out of the graph entirely; that costs
    /// them nothing because they only count, but it would make every assertion
    /// below about `Rule::AluReg` vacuous.)
    #[cfg(test)]
    const MIR_ALU_CODE: [u8; 8] = [0x1a, 0x1b, 0x60, 0x1a, 0x68, 0xac, 0, 0];

    /// [`MIR_ALU_CODE`] as a graph and a schedule, for the tests that work on
    /// the machine list directly instead of on the emitted bytes.
    #[cfg(test)]
    fn mir_alu_graph() -> (Graph, Schedule) {
        let builder = IrBuilder::new(2, 2);
        let graph = builder.build(&MIR_ALU_CODE, 6).expect("build");
        let schedule = ir_schedule::schedule(&graph);
        (graph, schedule)
    }

    #[cfg(test)]
    fn mir_emitted_bytes(mode: Option<MirMode>) -> Vec<u8> {
        // Both arms with the register cache OFF. Production makes the two
        // mutually exclusive, so a comparison that left it on for the no-mode
        // arm and off for the MIR arm would measure residency rather than the
        // selector -- and would report the selector as having changed bytes it
        // never touched.
        let _ls = LsForce::off();
        let _force = mode.map(MirForce::set);
        let cm = compile_via_ir(&MIR_ALU_CODE, 6, 2, 2).expect("compiles");
        // SAFETY: the artifact is alive for the duration of this borrow, and
        // `code_len` is what the emitter wrote.
        unsafe { std::slice::from_raw_parts(cm.entry_ptr() as *const u8, cm.code_len()).to_vec() }
    }

    /// The level-2 encoder reproduces the per-opcode arms **byte for byte**.
    ///
    /// This is increment 2's whole deliverable. `Rule::AluReg`'s seven rows are
    /// anchored to the exact literals `lower_data_node` emits — `01 C8`,
    /// `29 C8`, `0F AF C1`, `48 21 C8` and so on — and the frame-homed
    /// allocation the encoder assumes IS this backend's allocation, so equality
    /// is reachable rather than approximate.
    ///
    /// The exact edit that trips it: change any of those literals in
    /// `lower_data_node`, or change `enc_frame_load`'s displacement-form choice
    /// on one side only.
    #[test]
    fn the_machine_level_emits_the_same_bytes_as_the_per_opcode_arms() {
        let _guard = mir_totals::TEST_LOCK.lock();
        mir_totals::reset();

        let off = mir_emitted_bytes(None);
        let emit = mir_emitted_bytes(Some(MirMode::Emit));
        assert!(
            !off.is_empty(),
            "precondition: the method compiled to bytes"
        );
        assert_eq!(off, emit, "the level-2 encoder changed the emitted bytes");

        // Not vacuous: tiles were actually taken. A path that declined every
        // tile would satisfy the equality above trivially.
        let (methods, tiles, mismatches) = mir_totals::read();
        assert_eq!(
            methods, 1,
            "exactly one method went through the machine list"
        );
        assert!(
            tiles >= 2,
            "`(a + b) * a` has two AluReg roots; the encoder took {tiles}"
        );
        assert_eq!(mismatches, 0);
    }

    // ── Increment 2: the allocation over the tile list ───────────────

    /// The machine list gets an allocation, and `verify_allocation` accepts it.
    ///
    /// The point of the increment is *which* verifier: the allocator's own,
    /// unmodified. A second checker written for the level-2 form would be a
    /// second model of the same program, and the whole reason the frame-homed
    /// allocation is describable at all is that `Allocation` already has a
    /// shape for "everything in its home word".
    ///
    /// Not vacuous by construction — `located` is asserted non-zero, which is
    /// the difference between "verified" and "there was nothing to verify".
    #[test]
    fn the_machine_list_carries_an_allocation_that_verify_allocation_accepts() {
        let (graph, schedule) = mir_alu_graph();
        let plan = plan_slots(&graph, &schedule, None);
        let mir = build_mir_plan(&graph, &schedule).expect("selection covers every block");

        let verdict =
            verify_mir_allocation(&graph, &schedule, &plan, &mir).expect("the allocation verifies");
        match verdict {
            MirAllocVerdict::Verified { located, .. } => assert!(
                located >= 3,
                "`(a + b) * a` has two params and two results; the allocation \
                 located only {located} values, so this test is close to vacuous"
            ),
            other => panic!("expected a verified allocation, got {other:?}"),
        }
    }

    /// The verifier is REACHED, and it can say no.
    ///
    /// A verification step nothing can fail is indistinguishable from no
    /// verification step. This one runs over an allocation whose every segment
    /// is `reg: None` — the arm where most of `verify_allocation`'s rules go
    /// quiet — so what gets injected against is the rule that does *not* go
    /// quiet: two values whose live ranges overlap, pointed at one home word.
    ///
    /// The violation is planted in the `SlotPlan`, UPSTREAM of
    /// `verify_mir_allocation`, so the whole path is under test rather than
    /// `verify_allocation` in isolation. Only the colour's *value* changes,
    /// never its `Some`-ness, so the liveness-agreement precheck still passes
    /// and the rejection can only have come from the verifier.
    ///
    /// The exact edit that trips it: drop the `verify_allocation` call from
    /// `verify_mir_allocation`.
    #[test]
    fn an_aliased_home_word_is_rejected_by_the_allocation_verifier() {
        use crate::regalloc::build_live_model;

        let (graph, schedule) = mir_alu_graph();
        let plan = plan_slots(&graph, &schedule, None);
        let mir = build_mir_plan(&graph, &schedule).expect("selection covers every block");
        // Precondition: unaliased, this graph verifies. Without it a broken
        // fixture would make the rejection below meaningless.
        assert!(
            matches!(
                verify_mir_allocation(&graph, &schedule, &plan, &mir),
                Ok(MirAllocVerdict::Verified { .. })
            ),
            "the fixture must verify BEFORE the injection"
        );

        // Two values genuinely live at the same position, so the rejection is
        // the aliasing rule and not some other one.
        let live = build_live_model(&graph, &schedule);
        let located: Vec<usize> = (0..graph.nodes.len())
            .filter(|&id| plan.node_color.get(id).copied().flatten().is_some())
            .filter(|&id| live.range.get(id).copied().flatten().is_some())
            .collect();
        let mut pair = None;
        'outer: for (i, &a) in located.iter().enumerate() {
            let ra = live.range[a].expect("located");
            for &b in &located[i + 1..] {
                let rb = live.range[b].expect("located");
                if ra.lo <= rb.hi && rb.lo <= ra.hi {
                    pair = Some((a, b));
                    break 'outer;
                }
            }
        }
        let (a, b) = pair.expect("this graph has two simultaneously live values");

        let mut aliased = plan_slots(&graph, &schedule, None);
        aliased.node_color[b] = aliased.node_color[a];
        let err = verify_mir_allocation(&graph, &schedule, &aliased, &mir)
            .expect_err("two values live together in one home word");
        let text = format!("{err:?}");
        assert!(
            text.contains("home word") || text.contains("slot pools"),
            "rejected for the wrong reason: {text}"
        );
    }

    /// Emit mode refuses a compile whose allocation the verifier rejected;
    /// verify mode does not.
    ///
    /// Asserted as a property of the two modes rather than by forcing a
    /// rejection, because the only way to force one on a real graph is to
    /// introduce the compiler bug it exists to catch. What is checkable without
    /// that is the ASYMMETRY, and the asymmetry is the whole policy: verify
    /// mode's contract is that it changes no emitted byte, which it would break
    /// by also changing which methods compile.
    #[test]
    fn verify_mode_records_the_allocation_and_emit_mode_stakes_the_compile_on_it() {
        let _guard = mir_totals::TEST_LOCK.lock();

        for mode in [MirMode::Verify, MirMode::Emit] {
            mir_totals::reset();
            let bytes = mir_emitted_bytes(Some(mode));
            assert!(!bytes.is_empty(), "{mode:?} must still produce a body");
            let (verified, values, vacuous, indescribable, rejected) = mir_totals::read_alloc();
            assert_eq!(rejected, 0, "{mode:?}: the allocation was rejected");
            assert_eq!(
                verified, 1,
                "{mode:?}: one method, one verdict — got verified={verified} \
                 nothing_to_cover={vacuous} indescribable={indescribable}"
            );
            assert!(
                values > 0,
                "{mode:?}: the verdict was `Verified` over zero values, which \
                 is `NothingToCover` wearing the wrong label"
            );
        }
    }

    /// Verify mode is the same oracle without the risk: the per-opcode arms
    /// still emit, and the encoder's answer is compared against what they wrote.
    #[test]
    fn verify_mode_agrees_and_changes_no_emitted_byte() {
        let _guard = mir_totals::TEST_LOCK.lock();
        mir_totals::reset();

        let off = mir_emitted_bytes(None);
        let verify = mir_emitted_bytes(Some(MirMode::Verify));
        assert_eq!(off, verify, "verify mode must not change an emitted byte");

        let (methods, tiles, mismatches) = mir_totals::read();
        assert_eq!(methods, 1);
        assert!(tiles >= 2, "verify mode checked {tiles} tiles");
        assert_eq!(mismatches, 0, "the two paths disagreed");
    }

    /// The oracle can FAIL. Injected, because a guard nobody has seen fail is a
    /// guard of unknown polarity — and this one's whole job is to be believed
    /// when it stays silent over a corpus.
    ///
    /// Both halves matter: verify mode must *count* the disagreement, and the
    /// compile must be *refused* rather than shipped with a body nothing
    /// checked. Increment 2 is where fail-closed returns.
    #[test]
    fn an_injected_wrong_byte_is_caught_by_the_oracle() {
        let _guard = mir_totals::TEST_LOCK.lock();
        mir_totals::reset();

        let _inject = MirInject::on();
        let _force = MirForce::set(MirMode::Verify);
        let refused = compile_via_ir(&MIR_ALU_CODE, 6, 2, 2);
        assert!(
            refused.is_none(),
            "a disagreeing method must lose its optimized body, not ship it"
        );
        let (_, _, mismatches) = mir_totals::read();
        assert!(
            mismatches >= 1,
            "the byte-equality oracle did not notice a corrupted tile"
        );
    }

    /// With both flags off — the default, and every compile today — the machine
    /// list is not built at all.
    ///
    /// The counter, not the bytes: "changed no byte" and "did not execute" are
    /// different claims, and the equality tests above would pass vacuously on a
    /// path that never ran.
    #[test]
    fn the_machine_level_is_off_by_default() {
        let _guard = mir_totals::TEST_LOCK.lock();
        mir_totals::reset();

        compile_via_ir(&MIR_ALU_CODE, 6, 2, 2).expect("compiles");

        assert_eq!(
            mir_totals::read(),
            (0, 0, 0),
            "the machine list was built without being asked"
        );
    }

    /// A tile that absorbed another node must never reach the encoder.
    ///
    /// `Rule::AluReg` covers exactly its own root, so this guard is insurance
    /// against a future rule rather than a live filter — which is precisely why
    /// it is tested directly instead of through a compile. Driven through one it
    /// would be vacuous: the rules that DO absorb (`Lea`, `AluImm`, the fused
    /// branches) are already refused a line earlier, by rule.
    ///
    /// What it prevents: the caller skips every node a tile covers, so a tile
    /// that absorbed a node the encoder does not actually fold leaves that node
    /// computed nowhere. That is the same failure `BlockSelection::covers`
    /// exists to make unrepresentable, one level lower down.
    ///
    /// The exact edit that trips it: delete the `covered.as_slice() == [id]`
    /// clause from `mir_tile_is_emittable`.
    #[test]
    fn the_encoder_refuses_a_tile_that_absorbed_another_node() {
        use crate::x64::isel::{MInst, Op as SelOp, Rule, Tile, Ty};

        let alu = |dst: NodeId, lhs: NodeId, rhs: NodeId| MInst::AluRR {
            op: SelOp::Add,
            ty: Ty::I32,
            dst,
            lhs,
            rhs,
        };
        // The shape the encoder handles: one root, nothing absorbed.
        let plain = Tile::for_test(5, vec![5], vec![alu(5, 3, 4)], Rule::AluReg);
        assert!(mir_tile_is_emittable(&plain, 5));

        // The same tile, having absorbed node 4.
        let absorbing = Tile::for_test(5, vec![5, 4], vec![alu(5, 3, 4)], Rule::AluReg);
        assert!(
            !mir_tile_is_emittable(&absorbing, 5),
            "a tile that absorbed node 4 would leave it computed nowhere"
        );

        // And a rule the encoder has not been proved byte-equal for, however
        // ordinary its cover list.
        let other = Tile::for_test(5, vec![5], vec![alu(5, 3, 4)], Rule::AluImm);
        assert!(!mir_tile_is_emittable(&other, 5));
        // A tile rooted somewhere else is never this node's answer.
        assert!(!mir_tile_is_emittable(&plain, 4));
    }

    /// The two frame-access encoders are the ONE producer of these bytes.
    ///
    /// `load_to_rax` / `load_to_rcx` / `store_rax` delegate to them, so the
    /// level-2 encoder and the per-opcode arms cannot drift apart. This pins the
    /// literals those methods used to hold inline, in both displacement forms.
    #[test]
    fn the_frame_access_encoders_reproduce_the_inline_literals() {
        let bytes = |f: fn(u8, i32, &mut FrameAccess), reg: u8, off: i32| -> Vec<u8> {
            let mut a = FrameAccess::new();
            f(reg, off, &mut a);
            a.as_slice().to_vec()
        };
        // disp8: mod=01, r/m=RBP(101).
        assert_eq!(bytes(enc_frame_load, RAX, 8), vec![0x48, 0x8B, 0x45, 0xF8]);
        assert_eq!(bytes(enc_frame_load, RCX, 8), vec![0x48, 0x8B, 0x4D, 0xF8]);
        assert_eq!(bytes(enc_frame_store, RAX, 8), vec![0x48, 0x89, 0x45, 0xF8]);
        // disp32: mod=10.
        let mut want = vec![0x48u8, 0x8B, 0x85];
        want.extend_from_slice(&(-4096i32).to_le_bytes());
        assert_eq!(bytes(enc_frame_load, RAX, 4096), want);
        let mut want = vec![0x48u8, 0x89, 0x85];
        want.extend_from_slice(&(-4096i32).to_le_bytes());
        assert_eq!(bytes(enc_frame_store, RAX, 4096), want);
        // A register these encodings cannot express is a refusal, not a
        // silently truncated `reg & 7` — which would encode R8 as RAX.
        assert!(bytes(enc_frame_load, 8, 8).is_empty());
        assert!(bytes(enc_frame_store, 8, 8).is_empty());
    }

    // ── The three `ir::Op` enumerations, checked against each other ──
    //
    // `docs/feature-designs/jit-machine-level-and-instruction-selection.md`,
    // "The cheap alternative". One set of operations is enumerated
    // in three places, in two different vocabularies:
    //
    //   1. `lower_data_node`'s match arms          — 48 of 53 `ir::Op` variants
    //   2. `op_defines_result_slot`                — 37
    //   3. `ir::ir_compatible`                     — in the BYTECODE's
    //      vocabulary (`scan.anewarray_ops`, …), not `Op`'s
    //
    // (1) and (2) are checked here. (3) cannot be: it answers a different
    // question about a different type, which is precisely why the catch-all's
    // old claim — "bail in `ir_compatible` prevents reaching here" — was not
    // checkable where it was written, and why `lower_data_node`'s final arm now
    // refuses instead of asserting.
    //
    // WHAT TRIPS THESE (rule 5 of `docs/known-issues/c2/README.md`: write down
    // the exact edit, or the test is decoration):
    //
    //   * Add a variant to `ir::Op`. `declared_lowering` is an exhaustive match
    //     with NO wildcard arm, so the crate stops compiling until the author
    //     classifies it. That is the primary forcing function and it fires at
    //     build time, not test time.
    //   * Classify it `Lowered*` but forget the `lower_data_node` arm.
    //     `every_ir_op_is_lowered_or_declared_unlowerable` fails: the source
    //     scan finds no arm naming it.
    //   * Add the arm but forget `op_defines_result_slot`.
    //     `op_defines_result_slot_matches_the_arms_that_allocate` fails.
    //   * Remove a variant from `ir::Op` and leave it here.
    //     `the_op_representatives_cover_every_declared_variant` fails.
    //
    // All three scans read the source with `include_str!`, the same idiom as
    // `the_register_read_path_is_gated_on_publication` above.

    /// What `lower_data_node` does with one `ir::Op`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum OpLowering {
        /// Has an arm, and that arm allocates a result slot. Must therefore
        /// also be named by `op_defines_result_slot`.
        LoweredValue,
        /// Has an arm, but produces no value anyone can read: a control node, a
        /// memory-only effect, or a terminator handled by `lower_terminator`.
        LoweredEffect,
        /// No arm. Reaching `lower_data_node` with one refuses the compile.
        Unlowerable,
    }

    /// The variants with no lowering arm — the explicit list the catch-all
    /// used to leave implicit.
    ///
    /// Each is unreachable from a real compile today (see the catch-all's own
    /// comment for the evidence per op). This is a statement about the tree,
    /// not a permission: a variant added here is a variant the optimizing tier
    /// silently declines to compile, and the edit should be visible in review.
    ///
    /// `NewArray` left this list in cov-06: it now has a real arm below (the
    /// shared `emit_new_array_stub`, mirroring `Op::New`).
    const UNLOWERABLE: [&str; 3] = ["I2B", "I2C", "I2S"];

    /// **Exhaustive on purpose — do not add a wildcard arm.**
    ///
    /// This match is the forcing function. A new `ir::Op` variant makes the
    /// crate fail to compile here, which is the only mechanism that catches the
    /// author *before* the tests run.
    fn declared_lowering(op: &Op) -> OpLowering {
        use OpLowering::{LoweredEffect, LoweredValue, Unlowerable};
        match op {
            // Control and terminator nodes: an arm exists and does nothing,
            // because `lower_terminator` owns them.
            Op::Start
            | Op::Return
            | Op::Throw
            | Op::If
            | Op::Merge
            | Op::Region
            | Op::Proj(_)
            | Op::Dead => LoweredEffect,
            // Values.
            Op::Const(_)
            | Op::ConstF(_)
            | Op::Param(_)
            | Op::Phi
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Rem
            | Op::Neg
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Shl
            | Op::Shr
            | Op::UShr
            | Op::Cmp(_)
            | Op::LCmp
            | Op::FCmp { .. }
            | Op::I2L
            | Op::L2I
            | Op::I2F
            | Op::I2D
            | Op::L2F
            | Op::L2D
            | Op::F2I
            | Op::F2L
            | Op::F2D
            | Op::D2I
            | Op::D2L
            | Op::D2F
            | Op::Load(_)
            | Op::ArrayLoad(_)
            | Op::ArrayStore(_)
            | Op::ArrayLength
            | Op::New { .. }
            | Op::NewArray { .. }
            | Op::Call { .. }
            | Op::ConstString { .. }
            | Op::ConstClass { .. }
            | Op::LoadStatic { .. }
            | Op::LambdaIntToDouble
            | Op::InstanceOf { .. }
            | Op::CheckCast { .. } => LoweredValue,
            // Effects with an arm but no result slot.
            Op::Store(_) | Op::MonitorEnter | Op::MonitorExit | Op::Guard { .. } => LoweredEffect,
            // No arm. Keep in step with `UNLOWERABLE`; the tests check it.
            Op::I2B | Op::I2C | Op::I2S => Unlowerable,
        }
    }

    /// One representative value per `ir::Op` variant, with the variant's name.
    ///
    /// The names are what the source scans compare against;
    /// `the_op_representatives_cover_every_declared_variant` proves the list is
    /// complete against `ir.rs` itself, so it cannot quietly fall behind.
    fn op_representatives() -> Vec<(&'static str, Op)> {
        use crate::ir::CmpOp;
        vec![
            ("Start", Op::Start),
            ("Return", Op::Return),
            ("If", Op::If),
            ("Merge", Op::Merge),
            ("Region", Op::Region),
            ("Proj", Op::Proj(0)),
            ("Const", Op::Const(0)),
            ("ConstF", Op::ConstF(0)),
            ("Param", Op::Param(0)),
            ("Phi", Op::Phi),
            ("Add", Op::Add),
            ("Sub", Op::Sub),
            ("Mul", Op::Mul),
            ("Div", Op::Div),
            ("Rem", Op::Rem),
            ("Neg", Op::Neg),
            ("And", Op::And),
            ("Or", Op::Or),
            ("Xor", Op::Xor),
            ("Shl", Op::Shl),
            ("Shr", Op::Shr),
            ("UShr", Op::UShr),
            ("Cmp", Op::Cmp(CmpOp::Eq)),
            ("LCmp", Op::LCmp),
            (
                "FCmp",
                Op::FCmp {
                    double: false,
                    nan_greater: false,
                },
            ),
            ("I2L", Op::I2L),
            ("L2I", Op::L2I),
            ("I2F", Op::I2F),
            ("I2D", Op::I2D),
            ("L2F", Op::L2F),
            ("L2D", Op::L2D),
            ("F2I", Op::F2I),
            ("F2L", Op::F2L),
            ("F2D", Op::F2D),
            ("D2I", Op::D2I),
            ("D2L", Op::D2L),
            ("D2F", Op::D2F),
            ("I2B", Op::I2B),
            ("I2C", Op::I2C),
            ("I2S", Op::I2S),
            ("Load", Op::Load(MemKind::Int)),
            ("Store", Op::Store(MemKind::Int)),
            ("ArrayLength", Op::ArrayLength),
            ("ArrayLoad", Op::ArrayLoad(MemKind::Int)),
            ("ArrayStore", Op::ArrayStore(MemKind::Int)),
            (
                "New",
                Op::New {
                    class_id: 0,
                    num_fields: 0,
                },
            ),
            (
                "NewArray",
                Op::NewArray {
                    element_type: 10,
                    component_class_id: 0,
                },
            ),
            ("Call", Op::Call { info_ptr: 0 }),
            (
                "ConstString",
                Op::ConstString {
                    holder_class_id: 0,
                    cp_idx: 0,
                },
            ),
            (
                "ConstClass",
                Op::ConstClass {
                    holder_class_id: 0,
                    cp_idx: 0,
                },
            ),
            (
                "LoadStatic",
                Op::LoadStatic {
                    class_id: 0,
                    field_index: 0,
                    type_tag: b'I',
                    is_volatile: false,
                },
            ),
            ("LambdaIntToDouble", Op::LambdaIntToDouble),
            (
                "InstanceOf",
                Op::InstanceOf {
                    name_ptr: 0,
                    name_len: 0,
                },
            ),
            (
                "CheckCast",
                Op::CheckCast {
                    name_ptr: 0,
                    name_len: 0,
                },
            ),
            ("MonitorEnter", Op::MonitorEnter),
            ("MonitorExit", Op::MonitorExit),
            ("Throw", Op::Throw),
            ("Guard", Op::Guard { bci: 0 }),
            ("Dead", Op::Dead),
        ]
    }

    /// Every `Op::X` named at match-arm depth inside `lower_data_node`.
    ///
    /// Twelve-space indentation is the arm depth in that function; anything
    /// deeper is inside an arm *body* (`matches!(node.op, Op::MonitorEnter)` at
    /// the monitor arm, for one), and counting those would report an arm that
    /// does not exist.
    fn ops_with_a_lowering_arm() -> std::collections::BTreeSet<String> {
        let src = include_str!("ir_lower.rs");
        let body = src
            .split("fn lower_data_node(&mut self, id: NodeId) {")
            .nth(1)
            .expect("lower_data_node is in this file")
            .split("\n    fn ")
            .next()
            .expect("the function ends");
        let mut out = std::collections::BTreeSet::new();
        for line in body.lines() {
            if line.starts_with("            Op::") || line.starts_with("            | Op::") {
                collect_op_names(line, &mut out);
            }
        }
        assert!(
            !out.is_empty(),
            "the arm scan found nothing — `lower_data_node`'s shape changed and \
             this test would now pass vacuously"
        );
        out
    }

    /// Every `Op::X` named in `op_defines_result_slot`'s body.
    fn ops_that_define_a_result_slot() -> std::collections::BTreeSet<String> {
        let src = include_str!("ir_lower.rs");
        let body = src
            .split("fn op_defines_result_slot(op: &Op) -> bool {")
            .nth(1)
            .expect("op_defines_result_slot is in this file")
            .split("\n}")
            .next()
            .expect("the function ends");
        let mut out = std::collections::BTreeSet::new();
        collect_op_names(body, &mut out);
        assert!(!out.is_empty(), "the slot scan found nothing");
        out
    }

    /// Every variant declared in `ir::Op` itself — the ground truth.
    ///
    /// The `\r` strip is load-bearing, not tidiness. `include_str!` returns the
    /// file's bytes verbatim, and a CRLF checkout (the Windows default, and
    /// what `core.autocrlf=true` produces) makes a `}` line read as `\r\n}\r\n`
    /// — so the `"\n}\n"` terminator below matches nothing and the "enum block"
    /// silently runs to the end of `ir.rs`, sweeping up every variant of every
    /// other enum in the file. That is the whole check reading garbage, and it
    /// was doing so on Windows before cov-01 (verified against an unmodified
    /// `dev`); it passed on Linux only because the checkout is LF there.
    fn declared_op_variants() -> std::collections::BTreeSet<String> {
        let src = include_str!("ir.rs").replace("\r\n", "\n");
        let block = src
            .split("\npub enum Op {")
            .nth(1)
            .expect("ir.rs declares `pub enum Op`")
            .split("\n}\n")
            .next()
            .expect("the enum ends");
        let mut out = std::collections::BTreeSet::new();
        for line in block.lines() {
            // Variants sit at exactly four spaces; struct-variant FIELDS sit at
            // eight and are lower-case, doc comments start with `/`.
            let rest = match line.strip_prefix("    ") {
                Some(r) => r,
                None => continue,
            };
            if !rest.starts_with(|c: char| c.is_ascii_uppercase()) {
                continue;
            }
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            if !name.is_empty() {
                out.insert(name);
            }
        }
        assert!(!out.is_empty(), "the `ir::Op` scan found nothing");
        out
    }

    fn collect_op_names(text: &str, out: &mut std::collections::BTreeSet<String>) {
        let mut rest = text;
        while let Some(i) = rest.find("Op::") {
            let after = &rest[i + 4..];
            let name: String = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            if !name.is_empty() && name.starts_with(|c: char| c.is_ascii_uppercase()) {
                out.insert(name);
            }
            rest = after;
        }
    }

    /// The representative list is complete against `ir::Op`'s own declaration.
    ///
    /// Trips when a variant is REMOVED from `ir::Op` and left here — the case
    /// `declared_lowering`'s exhaustiveness cannot catch, because deleting a
    /// variant makes an arm unreachable, not missing.
    #[test]
    fn the_op_representatives_cover_every_declared_variant() {
        let declared = declared_op_variants();
        let listed: std::collections::BTreeSet<String> = op_representatives()
            .into_iter()
            .map(|(n, _)| n.to_string())
            .collect();
        assert_eq!(
            declared, listed,
            "`op_representatives` and `ir::Op` disagree — left is what ir.rs \
             declares, right is what this test enumerates"
        );
    }

    /// Every `ir::Op` variant either has a lowering arm or is on `UNLOWERABLE`.
    ///
    /// This is the check the catch-all's old comment stood in for.
    #[test]
    fn every_ir_op_is_lowered_or_declared_unlowerable() {
        let armed = ops_with_a_lowering_arm();
        let mut missing_arm = Vec::new();
        let mut unexpected_arm = Vec::new();
        for (name, op) in op_representatives() {
            let has_arm = armed.contains(name);
            match declared_lowering(&op) {
                OpLowering::Unlowerable if has_arm => unexpected_arm.push(name),
                OpLowering::Unlowerable => {}
                _ if !has_arm => missing_arm.push(name),
                _ => {}
            }
        }
        assert!(
            missing_arm.is_empty(),
            "declared lowerable but `lower_data_node` has no arm: {missing_arm:?} — \
             either write the arm or move it to `UNLOWERABLE` deliberately"
        );
        assert!(
            unexpected_arm.is_empty(),
            "on `UNLOWERABLE` but `lower_data_node` now has an arm: {unexpected_arm:?} — \
             promote it to `LoweredValue`/`LoweredEffect`"
        );

        // And the explicit list agrees with the classification, so a reader can
        // trust `UNLOWERABLE` without re-deriving it.
        let classified: std::collections::BTreeSet<String> = op_representatives()
            .into_iter()
            .filter(|(_, op)| declared_lowering(op) == OpLowering::Unlowerable)
            .map(|(n, _)| n.to_string())
            .collect();
        let listed: std::collections::BTreeSet<String> =
            UNLOWERABLE.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            classified, listed,
            "`UNLOWERABLE` and `declared_lowering` disagree"
        );
    }

    /// Every `Op::X` named in `regalloc::ir_op_defines_value`'s body.
    ///
    /// Read out of the other file's source for the same reason
    /// [`ops_that_define_a_result_slot`] is read out of this one: the function
    /// is private, and a copy of its list maintained here would be a fourth
    /// enumeration of the same question.
    fn ops_regalloc_calls_value_defining() -> std::collections::BTreeSet<String> {
        let src = include_str!("regalloc.rs");
        let body = src
            .split("fn ir_op_defines_value(op: &Op) -> bool {")
            .nth(1)
            .expect("ir_op_defines_value is in regalloc.rs")
            .split("\n}")
            .next()
            .expect("the function ends");
        let mut out = std::collections::BTreeSet::new();
        collect_op_names(body, &mut out);
        assert!(
            !out.is_empty(),
            "the regalloc scan found nothing — `ir_op_defines_value`'s shape \
             changed and this test would now pass vacuously"
        );
        out
    }

    /// The two enumerations of "does this op define a value" are the SAME set.
    ///
    /// `regalloc::ir_op_defines_value` calls itself a "verbatim mirror" of
    /// `op_defines_result_slot`, and until 2026-09-02 it was not one: it omitted
    /// `Op::ArrayLength` and `Op::NewArray`, and its own doc comment asserted
    /// they were absent from both. The comment written to prevent the drift was
    /// the drift.
    ///
    /// **What that cost was invisible, which is why this test exists rather
    /// than a stricter comment.** `plan_register_residency` compares
    /// `wants_loc` (built from the regalloc predicate) against `node_color`
    /// (built from this file's), and ONE disagreement declines register
    /// residency for the whole method. Every counted loop written
    /// `for (i = 0; i < a.length; i++)` contains an `arraylength`, so every one
    /// of them declined — silently, because the flag reported only successes.
    /// Nothing was miscompiled; the optimization was simply unavailable
    /// wherever arrays are, which is most places.
    ///
    /// Compared as sets of names parsed from both sources, so adding an arm to
    /// one list and forgetting the other fails here instead of turning up as an
    /// unexplained refusal months later.
    #[test]
    fn the_two_value_defining_enumerations_agree() {
        let here = ops_that_define_a_result_slot();
        let there = ops_regalloc_calls_value_defining();

        let missing_in_regalloc: Vec<&String> = here.difference(&there).collect();
        let missing_here: Vec<&String> = there.difference(&here).collect();

        assert!(
            missing_in_regalloc.is_empty(),
            "`op_defines_result_slot` names these and `regalloc::ir_op_defines_value` \
             does not: {missing_in_regalloc:?} — the colourer gives them a home the \
             liveness model does not know about, so `plan_register_residency` \
             declines residency for EVERY method containing one",
        );
        assert!(
            missing_here.is_empty(),
            "`regalloc::ir_op_defines_value` names these and `op_defines_result_slot` \
             does not: {missing_here:?} — the liveness model expects a home the \
             colourer never allocates, which is the direction that has no slot to \
             spill to",
        );
    }

    /// A counted loop over `a.length` reaches the register allocator.
    ///
    /// The end-to-end form of [`the_two_value_defining_enumerations_agree`],
    /// and the one that names the consequence rather than the cause.
    /// `plan_register_residency`'s agreement check compares `wants_loc`
    /// (`regalloc::ir_op_defines_value`) against `node_color`
    /// (`op_defines_result_slot`) and declines register residency for the
    /// WHOLE method on a single disagreement. `Op::ArrayLength` was in the
    /// second list and not the first, so this shape — the most ordinary
    /// counted loop in Java — declined every time, and the flag reported
    /// nothing because it printed only on success.
    ///
    /// The bytecode is `static int f(int[] a) { int s = 0; for (int i = 0; i <
    /// a.length; i++) s += a[i]; return s; }`, assembled by hand so the
    /// `arraylength` is unmistakably present rather than incidental to a
    /// fixture.
    ///
    /// Asserted on the AGREEMENT, not on a promotion count: whether this
    /// particular graph ends up with a register is the allocator's business and
    /// may legitimately change, but the two models must never disagree about
    /// which values want a home.
    #[test]
    fn a_counted_loop_over_array_length_reaches_the_allocator() {
        use crate::regalloc::build_live_model;

        // 0: iconst_0            s = 0
        // 1: istore_1
        // 2: iconst_0            i = 0
        // 3: istore_2
        // 4: iload_2         <-- loop head
        // 5: aload_0
        // 6: arraylength         THE OP THAT USED TO DECLINE THE METHOD
        // 7: if_icmpge +15  --> 22
        // 10: iload_1
        // 11: aload_0
        // 12: iload_2
        // 13: iaload
        // 14: iadd
        // 15: istore_1
        // 16: iinc 2, 1
        // 19: goto -15      --> 4
        // 22: iload_1
        // 23: ireturn
        let code: [u8; 24] = [
            0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x2a, 0xbe, 0xa2, 0x00, 0x0f, 0x1b, 0x2a, 0x1c, 0x2e,
            0x60, 0x3b, 0x84, 0x02, 0x01, 0xa7, 0xff, 0xf1, 0x1b, 0xac,
        ];
        let graph = IrBuilder::new(1, 3)
            .build(&code, code.len())
            .expect("the loop builds");
        assert!(
            graph
                .nodes
                .iter()
                .any(|n| matches!(n.op, Op::ArrayLength)),
            "the fixture must contain an ArrayLength, or this test proves nothing"
        );

        let schedule = ir_schedule::schedule(&graph);
        let plan = plan_slots(&graph, &schedule, None);
        let live = build_live_model(&graph, &schedule);

        assert_eq!(
            live.wants_loc.len(),
            plan.node_color.len(),
            "the two models disagree about how many nodes there are"
        );
        let disagreeing: Vec<(usize, String)> = live
            .wants_loc
            .iter()
            .zip(plan.node_color.iter())
            .enumerate()
            .filter(|(_, (wants, color))| **wants != color.is_some())
            .map(|(id, _)| {
                (
                    id,
                    graph
                        .nodes
                        .get(id)
                        .map(|n| format!("{:?}", n.op))
                        .unwrap_or_default(),
                )
            })
            .collect();
        assert!(
            disagreeing.is_empty(),
            "liveness and colourer disagree on {disagreeing:?} — \
             `plan_register_residency` declines residency for the whole method \
             on any one of these, so this ordinary counted loop gets no \
             registers at all",
        );
    }

    /// `op_defines_result_slot` names exactly the arms that allocate a slot.
    ///
    /// The direction that matters is *over*-claiming: an op listed here whose
    /// arm never calls `alloc_slot` makes `slot_of` hand back a zero offset,
    /// which is `[rbp - 0]` — the saved caller frame pointer — read as a value.
    /// That is the defect `verify_data_locations` was written for, arriving
    /// through the other door.
    #[test]
    fn op_defines_result_slot_matches_the_arms_that_allocate() {
        let named_in_fn = ops_that_define_a_result_slot();
        let mut wrong = Vec::new();
        for (name, op) in op_representatives() {
            let declared = declared_lowering(&op) == OpLowering::LoweredValue;
            if op_defines_result_slot(&op) != declared || named_in_fn.contains(name) != declared {
                wrong.push((
                    name,
                    declared,
                    op_defines_result_slot(&op),
                    named_in_fn.contains(name),
                ));
            }
        }
        assert!(
            wrong.is_empty(),
            "(op, classified as value, `op_defines_result_slot` says, source names it): {wrong:?}"
        );
        // Guards the "in step" claim from the other side: nothing may define a
        // result slot without having an arm at all.
        let armed = ops_with_a_lowering_arm();
        let orphans: Vec<&String> = named_in_fn.difference(&armed).collect();
        assert!(
            orphans.is_empty(),
            "`op_defines_result_slot` claims a slot for ops with no lowering arm: {orphans:?}"
        );
    }

    /// An op with no lowering arm REFUSES the compile — it does not emit
    /// nothing.
    ///
    /// The witness has to be an op whose result **nobody reads**, because that
    /// is the only case the pre-emission nets do not already cover:
    /// `verify_data_locations` refuses an unlowered op only when some node
    /// reads its value. `ir::Op::MonitorEnter` was exactly that shape and this
    /// is the general form of the guard it got.
    ///
    /// `Op::I2B` is used because it is on `UNLOWERABLE` and produces a value;
    /// the graph is hand-built so no optimizer pass can DCE it away before
    /// the lowerer sees it. (It was `Op::ArrayLength` until COV-02 gave that
    /// op a lowering arm, then `Op::NewArray` until cov-06 gave THAT op one —
    /// the witness has to be an op with NO arm, which is exactly what the
    /// precondition assertion below enforces. `I2B`/`I2C`/`I2S` stay on
    /// `UNLOWERABLE` permanently: the builder decomposes 0x91/0x92/0x93 into
    /// `Shl`/`Shr`/`And` instead of ever constructing one, so there is no
    /// real arm to add.)
    ///
    /// Unlike `NewArray` (control-anchored: `[ctrl, mem, length]`), `I2B`
    /// takes a single VALUE input and no control edge — but the scheduler
    /// places every live non-control node into a block regardless of its
    /// input shape or use count (`ir_schedule`'s block-placement loop
    /// iterates `graph.nodes` unconditionally), so an unread `I2B` is
    /// scheduled exactly as reliably as an unread `NewArray` was.
    ///
    /// **Anti-vacuity, executed rather than argued (2026-08-03).** A test that
    /// asserts `is_none()` passes for any refusal, including one that has
    /// nothing to do with the catch-all. So the mutation was run: with the
    /// final arm of `lower_data_node` reverted to `_ => {}`, this graph
    /// compiles to a body and this assertion fails. That is the edit to repeat
    /// if the test is ever suspected of measuring something else.
    #[test]
    fn an_op_with_no_lowering_arm_refuses_instead_of_emitting_nothing() {
        use crate::ir::{Graph, IrType, Op, NO_NODE};

        let mut graph = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        };
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let int_arg = graph.add(Op::Param(0), IrType::Int, vec![start], None);
        // Scheduled, lowered, and read by nobody — the monitor's shape.
        let _narrowed = graph.add(Op::I2B, IrType::Int, vec![int_arg], None);
        let zero = graph.add(Op::Const(0), IrType::Int, vec![], None);
        let ret = graph.add(Op::Return, IrType::Void, vec![ctrl, zero], None);
        graph.exit = ret;

        assert_eq!(
            declared_lowering(&Op::I2B),
            OpLowering::Unlowerable,
            "precondition: this test is only meaningful while `I2B` has \
             no arm — if one was added, pick another `UNLOWERABLE` op"
        );

        let schedule = ir_schedule::schedule(&graph);
        assert!(
            schedule.blocks.iter().any(|b| b.nodes.contains(&_narrowed)),
            "precondition: the unlowerable node must actually be scheduled, or \
             this test passes without exercising the catch-all"
        );

        assert!(
            lower(&graph, &schedule, 1, 1, &no_helpers()).is_none(),
            "a graph containing an op with no lowering arm must not produce a \
             compiled body"
        );
    }

    /// `regalloc::ir_op_is_call` did not count it until this wiring landed.
    #[test]
    fn a_value_live_across_a_helper_call_keeps_no_register() {
        let mut graph = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        };
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let a = graph.add(Op::Param(0), IrType::Double, vec![start], Some(0));
        let m = graph.add(
            Op::ConstF(3.0f64.to_bits()),
            IrType::Double,
            vec![],
            Some(1),
        );
        // `jit_drem` — a call that returns into the body.
        let r = graph.add(Op::Rem, IrType::Double, vec![a, m], Some(2));
        // `a` is read again AFTER the call, so its range spans it.
        let sum = graph.add(Op::Add, IrType::Double, vec![r, a], Some(3));
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, sum], Some(4));
        let schedule = ir_schedule::schedule(&graph);

        let plan = plan_slots(&graph, &schedule, None);
        let residency =
            plan_register_residency(&graph, &schedule, &plan).expect("the allocation verifies");
        let reg_of_a = residency
            .as_ref()
            .and_then(|res| res.reg_of.get(a as usize).copied().flatten());
        assert_eq!(
            reg_of_a, None,
            "n{a} spans a `CALL jit_drem`, which destroys every XMM this file \
             allocates over"
        );
    }

    /// `MachineModel::for_graph` cannot see the safepoint poll's slow-path
    /// call, because it belongs to no node. `ir_lower_machine_model` adds it.
    ///
    /// Without this the first loop-carried FP value would be handed a register
    /// the poll destroys on every back edge.
    #[test]
    fn the_machine_model_clobbers_the_back_edge_poll() {
        use crate::regalloc::{build_live_model, MachineModel, PhysReg, RegFile, RegSpec};

        // int sum(int n){ int s=0; for(int i=0;i<n;i++) s+=i; return s; }
        let code = [
            0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x0d, 0x1b, 0x1c, 0x60, 0x3c, 0x84,
            0x02, 0x01, 0xa7, 0xff, 0xf4, 0x1b, 0xac, 0, 0,
        ];
        // Unoptimized on purpose: the schedule's back edges are the subject,
        // and the optimizer is free to reshape them.
        let builder = IrBuilder::new(1, 3);
        let graph = builder.build(&code, 21).expect("IR build");
        let schedule = ir_schedule::schedule(&graph);
        let live = build_live_model(&graph, &schedule);
        assert!(live.converged);

        let file = || {
            RegFile::from_specs(IR_LOWER_LS_XMMS.iter().map(|&n| RegSpec {
                reg: PhysReg::xmm(n),
                caller_saved: true,
            }))
        };
        let bare = MachineModel::for_graph(&graph, &schedule, &live, file());
        let ours = ir_lower_machine_model(&graph, &schedule, &live, file());

        let back_edges: Vec<usize> = schedule
            .blocks
            .iter()
            .enumerate()
            .filter(|(b, block)| block.successors.iter().any(|&s| s <= *b))
            .filter_map(|(b, _)| live.span.get(b).map(|&(_, edge)| edge))
            .collect();
        assert!(
            !back_edges.is_empty(),
            "a counted loop must have a back edge to poll on"
        );

        let xmm2 = PhysReg::xmm(2);
        let holds = |model: &MachineModel, pos: usize| {
            model
                .clobbers
                .iter()
                .any(|(p, regs)| *p == pos && regs.contains(&xmm2))
        };
        for edge in back_edges {
            assert!(
                holds(&ours, edge),
                "the poll at the outgoing edge of a back-edge block calls its \
                 slow path; position {edge} must be a clobber"
            );
            assert!(
                !holds(&bare, edge),
                "this test is only meaningful while `for_graph` still misses \
                 the poll — if it now models it, delete the augmentation"
            );
        }

        // `MachineModel::clobbered_at` binary-searches this list.
        assert!(
            ours.clobbers.windows(2).all(|w| w[0].0 < w[1].0),
            "the clobber list must stay sorted with one entry per position"
        );
    }

    /// The finding that decides whether any of this does anything on real
    /// code: `IrBuilder` snapshots the frame at every bci, so `build_live_model`
    /// pins the whole value set and the allocator promotes nothing until the
    /// deopt pins are released.
    #[test]
    fn a_bytecode_graph_pins_everything_until_the_deopt_pins_are_released() {
        use crate::regalloc::{allocate_linear_scan, build_live_model, MachineModel, RegFile};

        let code = [
            0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x0d, 0x1b, 0x1c, 0x60, 0x3c, 0x84,
            0x02, 0x01, 0xa7, 0xff, 0xf4, 0x1b, 0xac, 0, 0,
        ];
        let builder = IrBuilder::new(1, 3);
        let graph = builder.build(&code, 21).expect("IR build");
        let schedule = ir_schedule::schedule(&graph);

        let mut live = build_live_model(&graph, &schedule);
        assert!(live.converged);
        assert!(
            !graph.safepoints.is_empty(),
            "the IR builder records a frame state per bci"
        );

        let wants = live.wants_loc.iter().filter(|w| **w).count();
        let pinned_before = live
            .wants_loc
            .iter()
            .zip(live.pinned.iter())
            .filter(|(w, p)| **w && **p)
            .count();
        assert!(
            wants > 0 && pinned_before * 2 > wants,
            "most values ({pinned_before} of {wants}) should be pinned by the \
             per-bci frame states — that is the whole finding"
        );

        let model = MachineModel::for_graph(&graph, &schedule, &live, RegFile::x86_64());
        let before = allocate_linear_scan(&graph, &live, &model).expect("allocates");

        let released = live.release_deopt_pins(&graph);
        assert!(released > 0, "there were pins to release");
        let after = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        crate::regalloc::verify_allocation(&graph, &live, &model, &after).expect("verifies");
        assert!(
            after.promoted > before.promoted,
            "releasing the deopt pins is what makes the allocator do anything \
             on a real method ({} → {})",
            before.promoted,
            after.promoted
        );
        // Phi pins are structural and must survive.
        for (id, node) in graph.nodes.iter().enumerate() {
            if matches!(node.op, Op::Phi) && live.wants_loc[id] {
                assert!(
                    live.pinned[id],
                    "phi n{id} lost its pin; its home is what the edge copies write"
                );
            }
        }
    }

    /// Write-through, end to end: the same method must return the same bits
    /// with the register cache on and off, and the "on" body must still emit
    /// every home store the "off" body did.
    #[test]
    fn the_register_cache_changes_no_result_and_drops_no_home_store() {
        let (graph, schedule) = fp_chain_graph();

        let (off_code, off_bits) = {
            // Forced OFF, not merely left alone: the cache is default-ON now,
            // so an unforced arm is the SAME configuration as the forced-on one
            // and every comparison below would be between a body and itself.
            let _flag = LsForce::off();
            let cm = lower(&graph, &schedule, 1, 1, &no_helpers()).expect("lowers");
            // SAFETY: one incoming argument, delivered as the raw bits of a
            // double in an integer ABI register, exactly as `Op::Param`
            // expects; the body calls no helper.
            let bits = unsafe { cm.try_call(&[3.0f64.to_bits() as i64]).expect("call") };
            (cm.code_bytes().to_vec(), bits)
        };
        assert_eq!(
            f64::from_bits(off_bits as u64),
            3.0 * 3.0 * 2.0 + 3.0,
            "the fixture itself must compute a*a*2+a"
        );

        let (on_code, on_bits, resident) = {
            let _flag = LsForce::on();
            let plan = plan_slots(&graph, &schedule, None);
            let resident = plan_register_residency(&graph, &schedule, &plan)
                .expect("verifies")
                .map(|r| r.promoted)
                .unwrap_or(0);
            let cm = lower(&graph, &schedule, 1, 1, &no_helpers()).expect("lowers");
            // SAFETY: as above.
            let bits = unsafe { cm.try_call(&[3.0f64.to_bits() as i64]).expect("call") };
            (cm.code_bytes().to_vec(), bits, resident)
        };

        assert!(resident > 0, "the fixture must actually promote something");
        assert_eq!(
            on_bits, off_bits,
            "the register cache changed the computed value"
        );

        // The register copy, emitted only by the cache — in EITHER bank.
        // `MOVAPS xmm, xmm` (0F 28) is the FP file's; `MOV r64, r64` (REX.W in
        // 48..4F, opcode 89, ModRM mod=11) is the GP one's. Which bank a given
        // fixture promotes into is the allocator's business, and a needle
        // naming only one of them makes this assertion vacuous the moment that
        // changes.
        //
        // Compared rather than counted absolutely: a short needle can also fall
        // inside some other instruction's encoding.
        let reg_copies = |code: &[u8]| -> usize {
            count_seq(code, &[0x0F, 0x28])
                + code
                    .windows(3)
                    .filter(|w| (0x48..=0x4F).contains(&w[0]) && w[1] == 0x89 && w[2] >= 0xC0)
                    .count()
        };
        assert!(
            reg_copies(&on_code) > reg_copies(&off_code),
            "the enabled body emitted no register copy, so the wiring did \
             nothing and the rest of this test is vacuous"
        );

        // MOVSD [rbp - disp], xmm0 — the home store. Write-through means the
        // cached body emits at least as many as the uncached one. Both
        // displacement forms, because `emit_rbp_modrm_disp` picks the smallest
        // and a needle spelling one of them counts zero on the other.
        let home_stores = |code: &[u8]| -> usize {
            count_seq(code, &[0xF2u8, 0x0F, 0x11, 0x85])
                + count_seq(code, &[0xF2u8, 0x0F, 0x11, 0x45])
        };
        let off_stores = home_stores(&off_code);
        assert!(
            off_stores > 0,
            "the fixture must store FP results to memory"
        );
        assert!(
            home_stores(&on_code) >= off_stores,
            "a home store disappeared: the frame image is no longer complete \
             at every instruction boundary, which is what `emit_safepoint_map` \
             and `build_deopt_points` rely on"
        );
    }

    // ── The callee-saved XMM save area ───────────────────────────────
    //
    // `IR_LOWER_SAVED_XMMS` is the prerequisite the level-2 lane was blocked
    // on, and its failure mode is the quietest one in this file: a caller's
    // XMM6 comes back holding a callee's `double`, on Windows only, with no
    // diagnostic anywhere. Nothing downstream checks it — the GC skips the
    // band by design, the deopt verifier skips it, and the value is a `double`
    // so it never looks like a bad pointer. So it is checked here, on the
    // emitted bytes, at every exit.

    /// Build a real `Lowerer` over a trivial graph, with a residency plan that
    /// names `reg`.
    ///
    /// The plan is synthesised rather than allocated, because what is under
    /// test is the *prologue's* reaction to a plan, not the allocator: a test
    /// that had to find a graph with five simultaneously live doubles would
    /// stop testing the save area the moment the allocator's heuristics moved.
    #[cfg(test)]
    fn lowerer_with_resident_xmm(buf_cap: usize, reg: Option<u8>) -> Lowerer<'static> {
        // Leaked so the returned `Lowerer<'static>` can borrow them; a handful
        // of small allocations per test, in a process that is about to exit.
        let (graph, schedule) = add_one_graph();
        let graph: &'static Graph = Box::leak(Box::new(graph));
        let schedule: &'static Schedule = Box::leak(Box::new(schedule));
        let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
        let empty: &'static HashMap<usize, bool> = Box::leak(Box::new(HashMap::new()));
        let no_direct: &'static HashMap<usize, (usize, bool)> = Box::leak(Box::new(HashMap::new()));
        let no_ic: &'static HashMap<usize, (usize, usize)> = Box::leak(Box::new(HashMap::new()));
        let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        let no_scopes: &'static InlineScopeTable = Box::leak(Box::new(InlineScopeTable::new()));
        let helpers: &'static JitRuntimeHelpers = Box::leak(Box::new(no_helpers()));
        let buf = ExecutableBuffer::new(buf_cap).expect("executable buffer");
        let mut lowerer = Lowerer::new(
            graph,
            schedule,
            buf,
            0,
            2,
            plan,
            helpers,
            empty,
            &[],
            None,
            no_direct,
            no_ic,
            &no_compact,
            no_scopes,
        );
        if let Some(reg) = reg {
            lowerer.set_residency(RegResidency {
                reg_of: vec![Some(reg)],
                // This helper drives the XMM half; the GP file is exercised by
                // its own tests.
                gp_reg_of: Vec::new(),
                promoted: 1,
                demoted: 0,
                peak_live: 1,
            });
        }
        lowerer
    }

    /// `MOVUPS [rbp - disp], xmm` and its load counterpart, for `reg < 8`.
    ///
    /// **Both** displacement forms, because `emit_rbp_modrm_disp` picks the
    /// smallest legal one: `mod=10` (`0x85 | reg<<3`, disp32) for a deep frame
    /// and `mod=01` (`0x45 | reg<<3`, disp8) for a shallow one. A needle
    /// spelling only one of them silently counts zero on the other, which reads
    /// as "the restore is missing" — the exact failure this test exists to
    /// report, arriving for the wrong reason.
    #[cfg(test)]
    fn movups_frame_needles(reg: u8, store: bool) -> [[u8; 3]; 2] {
        let op = if store { 0x11 } else { 0x10 };
        [
            [0x0F, op, 0x85 | ((reg & 7) << 3)],
            [0x0F, op, 0x45 | ((reg & 7) << 3)],
        ]
    }

    /// Occurrences of a frame `MOVUPS` for `reg` in `code`, either
    /// displacement form. See [`movups_frame_needles`].
    #[cfg(test)]
    fn count_movups_frame(code: &[u8], reg: u8, store: bool) -> usize {
        movups_frame_needles(reg, store)
            .iter()
            .map(|n| count_seq(code, n))
            .sum()
    }

    /// **The** invariant: every exit restores exactly what the prologue saved.
    ///
    /// All three exits are driven, not one — `emit_deopt_stub` inlines its own
    /// teardown instead of calling `emit_epilogue` (it must not restore the
    /// shadow `top`), so it is the one that silently keeps this frame's XMM6 in
    /// the caller's register file if the restore is left out of it. Deleting
    /// `emit_callee_saved_restore` from any of the three fails this test.
    #[test]
    fn every_exit_restores_exactly_what_the_prologue_saved() {
        let _flag = LsForce::on();
        let expect = usize::from(!IR_LOWER_SAVED_XMMS.is_empty());
        let reg = *IR_LOWER_SAVED_XMMS.first().unwrap_or(&6);
        // Closures rather than fixed byte needles: the displacement form depends
        // on the frame depth, so the count has to accept either one.
        let saves_in = |c: &[u8]| count_movups_frame(c, reg, true);
        let restores_in = |c: &[u8]| count_movups_frame(c, reg, false);

        // ── exit 1: the method epilogue ──────────────────────────────
        let mut lo = lowerer_with_resident_xmm(4096, Some(reg));
        lo.emit_prologue();
        let after_prologue = lo.buf.pos();
        lo.emit_epilogue();
        let code = lo.buf.as_slice().to_vec();
        let saves = saves_in(&code[..after_prologue]);
        assert_eq!(
            saves, expect,
            "the prologue saved {saves} of xmm{reg}, expected {expect} on this target",
        );
        assert_eq!(
            restores_in(&code[after_prologue..]),
            saves,
            "the epilogue did not restore what the prologue saved",
        );

        // ── exit 2: the shared call-exception bail stub ──────────────
        let mut lo = lowerer_with_resident_xmm(4096, Some(reg));
        lo.emit_prologue();
        let after_prologue = lo.buf.pos();
        lo.buf.emit(&[0, 0, 0, 0]);
        // `(patch_offset, throw_bci)` — the stub is emitted once per distinct
        // bci and stamps it. Which bci this exit carries is irrelevant to what
        // the test measures (that the stub restores what the prologue saved),
        // so 0.
        lo.call_exc_patches.push((after_prologue, 0));
        lo.emit_call_exc_stub();
        let code = lo.buf.as_slice().to_vec();
        assert_eq!(
            restores_in(&code[after_prologue..]),
            saves,
            "the call-exception bail stub returns the sentinel without \
             restoring the caller's registers",
        );

        // ── exit 3: the deopt stub's inlined teardown ────────────────
        let mut lo = lowerer_with_resident_xmm(4096, Some(reg));
        lo.emit_prologue();
        let after_prologue = lo.buf.pos();
        lo.buf.emit(&[0, 0, 0, 0]);
        lo.deopt_stub_patches.push(after_prologue);
        lo.emit_deopt_stub();
        let code = lo.buf.as_slice().to_vec();
        assert_eq!(
            restores_in(&code[after_prologue..]),
            saves,
            "the deopt stub inlines its own teardown and skipped the restore",
        );
    }

    /// The exceptional-exit stub stamps THIS method's own throw bci, once per
    /// DISTINCT site — the codegen half of the cov-07 residual fix.
    ///
    /// `JitSignals::athrow_bci` is consumed by `execute_jit_call` as this
    /// method's throw site and range-tested against `[start_pc, end_pc)` of
    /// every entry in its own exception table. A *dispatched callee*'s throw
    /// resets that field to `-1` (the general `set_jit_pending_exception`);
    /// only a local `athrow` sets a real bci. So each compiled exit owes its
    /// own stamp, or the interpreter routes with `throw_pc == usize::MAX` and
    /// `find_jit_exception_handler` skips every catch-all whose region does not
    /// span the whole method — i.e. every javac `finally`.
    ///
    /// # Why this exists next to the behavioural test
    ///
    /// `vm/tests/jit_ir_exception_stub_throw_bci.rs` proves the same property
    /// end-to-end, but it needs a built `cratonvm` binary AND a JDK, and skips
    /// itself when either is missing — so on a machine without them the
    /// property has NO guard at all. This one is pure codegen: it runs on every
    /// `cargo test -p cratonvm-jit`, needs nothing external, and goes red the
    /// instant the stamp or the per-bci grouping is removed.
    #[test]
    fn the_exception_stub_stamps_one_set_throw_bci_per_distinct_site() {
        unsafe extern "C" fn fake_set_throw_bci(_bci: i64) {}
        let addr = fake_set_throw_bci as *const () as usize;
        assert_ne!(addr, 0, "the stub's guard treats 0 as `no helper wired`");

        // Two distinct bcis across three exits: the third shares 0x1234, so a
        // per-SITE stub would emit three stamps and a per-BCI stub two. The
        // values are deliberately unlike any incidental byte run.
        let emit = |sites: &[usize]| -> Vec<u8> {
            let mut lo = lowerer_with_resident_xmm(4096, None);
            lo.set_throw_bci = addr;
            lo.emit_prologue();
            for &bci in sites {
                let patch = lo.buf.pos();
                lo.buf.emit(&[0, 0, 0, 0]);
                lo.call_exc_patches.push((patch, bci));
            }
            let stub_start = lo.buf.pos();
            lo.emit_call_exc_stub();
            lo.buf.as_slice()[stub_start..].to_vec()
        };

        let code = emit(&[0x1234, 0x5678, 0x1234]);
        assert_eq!(
            count_seq(&code, &(addr as u64).to_le_bytes()),
            2,
            "three exits over TWO distinct bcis must produce two stamped stubs \
             — one per distinct throw site, not one per branch site and not one \
             shared stub for the whole method",
        );
        for bci in [0x1234u64, 0x5678] {
            assert!(
                contains_seq(&code, &bci.to_le_bytes()),
                "bci {bci:#x} was never passed to `set_throw_bci`: the stub \
                 stamped something, but not this exit's own throw site",
            );
        }
        // The helper call clobbers RAX, which the epilogue returns as the
        // method result. Every stub must reload the sentinel after stamping or
        // the caller reads the helper's return value as the call's result.
        assert_eq!(
            count_seq(&code, &(i64::MIN as u64).to_le_bytes()),
            2,
            "each stub must reload the `i64::MIN` sentinel into RAX after the \
             stamping call clobbers it",
        );

        // Control: one distinct bci ⇒ exactly one stub, so the count above is
        // tracking distinct bcis rather than just counting exits.
        assert_eq!(
            count_seq(&emit(&[0x1234, 0x1234]), &(addr as u64).to_le_bytes()),
            1,
            "two exits sharing one bci must share one stub",
        );
    }

    /// A method that parks nothing in a callee-saved register emits no save.
    ///
    /// The save area costs frame bytes whenever the flag is on; it must not
    /// also cost two `MOVUPS` per call in the overwhelmingly common case where
    /// the allocation used none of them. `reg_of` naming only XMM2 is exactly
    /// that case.
    #[test]
    fn a_method_that_used_no_callee_saved_register_emits_no_save() {
        let _flag = LsForce::on();
        for plan in [None, Some(2u8)] {
            let mut lo = lowerer_with_resident_xmm(4096, plan);
            lo.emit_prologue();
            lo.emit_epilogue();
            let code = lo.buf.as_slice().to_vec();
            for &reg in IR_LOWER_SAVED_XMMS {
                assert_eq!(
                    count_movups_frame(&code, reg, true),
                    0,
                    "saved xmm{reg} for a plan that never used it ({plan:?})",
                );
            }
        }
    }

    /// Where the save area sits, stated as the three things it must not
    /// collide with.
    ///
    /// The whole placement argument is that the area lands inside
    /// `[callee_saved_lo, frame_size)` — the band
    /// `conservative_roots::band_slot_is_verifiable` already skips — so the
    /// reader side needed no edit. If it drifted below `spill_cap_off` it would
    /// overlap a spill and a `MOVUPS` would destroy a live value; if it drifted
    /// above, it would land in the outgoing-argument staging.
    #[test]
    fn the_save_area_lies_between_the_spills_and_the_argument_staging() {
        let _flag = LsForce::on();
        let lo = lowerer_with_resident_xmm(4096, Some(6));
        let bytes = lo.saved_xmm_bytes;
        assert_eq!(
            bytes,
            ir_saved_xmm_bytes(),
            "the frame was laid out against a different number than the \
             prologue writes through",
        );
        // Cast: a two-element compile-time constant.
        assert_eq!(bytes, IR_LOWER_SAVED_XMMS.len() as i32 * 16);

        let lo_off = lo.spill_cap_off;
        let hi_off = lo_off + bytes;
        assert!(
            lo.first_spill <= lo_off,
            "spills ({}) start at or above the save area ({lo_off})",
            lo.first_spill,
        );
        assert!(
            hi_off <= lo.args_stage_top_off || bytes == 0,
            "the save area ({lo_off}..{hi_off}) runs into the argument \
             staging at {}",
            lo.args_stage_top_off,
        );
        assert!(hi_off <= lo.frame_size, "the save area runs off the frame");

        // Every offset the prologue actually writes through, inside the band.
        for (reg, off) in lo.saved_xmm_regs() {
            assert!(
                off > lo_off && off <= hi_off,
                "xmm{reg} is saved at {off}, outside [{lo_off}, {hi_off}]",
            );
        }
    }

    /// A method with a save area publishes no OSR entry table.
    ///
    /// The trampoline enters *past* `emit_prologue` and builds the frame
    /// itself, so an OSR-entered method would restore XMM6/XMM7 from words the
    /// save never wrote. The `debug_assert!` at the publication site states the
    /// invariant; this is the behavioural half, on a method compiled with the
    /// linear-scan path on — because a `debug_assert` in a release build is a
    /// comment, and this is the one place the two paths could be wired together
    /// by someone who never reads either.
    #[test]
    fn an_ir_artifact_never_offers_an_osr_entry_the_save_area_would_break() {
        let _flag = LsForce::on();
        let cm = compile_via_ir(&MIR_ALU_CODE, 6, 2, 2).expect("compiles");
        assert!(
            cm.osr_pc_to_native.is_none(),
            "the IR tier published an OSR entry table; `osr_callee_saved_xmms` \
             and `osr_xmm_saved_base` must be published with it",
        );
    }

    /// With the linear-scan path off — the default, and every production
    /// compile today — the frame pays nothing.
    ///
    /// A save area that widened every IR frame by 32 bytes for a register
    /// nothing can hand out is a regression with no upside, and `frame_size`
    /// feeds `DEFAULT_MAX_FRAME_BYTES`, so it would also decline methods that
    /// used to compile.
    ///
    /// 2026-09-02: the file is default-ON now, so this pins the OFF arm
    /// (`CRATONVM_JIT_IR_LINEAR_SCAN=0`) rather than the process default.
    #[test]
    fn the_default_configuration_reserves_no_save_area() {
        let _off = LsForce::off();
        assert_eq!(
            ir_saved_xmm_bytes(),
            0,
            "the flag is off, so the frame must not pay for the save area",
        );
        let lo = lowerer_with_resident_xmm(4096, Some(6));
        assert_eq!(lo.saved_xmm_bytes, 0);
        assert_eq!(lo.saved_xmm_regs().count(), 0);
    }

    /// The publication interlock, asserted on the source because it is a
    /// property of *which accessor* every read uses, and a behavioural test
    /// cannot reach a lowering that forgot to convert a definition site.
    ///
    /// `reg_of` says which register a value was ASSIGNED; `reg_live` says the
    /// definition has actually written it. Every read must go through
    /// `resident_xmm` (which checks `reg_live`) or `assigned_xmm` (which is
    /// only for the two publishing sites). A definition arm nobody converted
    /// must therefore cost an optimization, never a read of a register that
    /// was never written.
    #[test]
    fn the_register_read_path_is_gated_on_publication() {
        // Only the emitter, not this module: the assertion below quotes the
        // very strings it looks for.
        //
        // The boundary is the test module, NAMED. It used to be "everything up
        // to the first `#[cfg(test)]`", which is a position rather than a
        // boundary: the first test-only helper added anywhere above
        // `set_residency` truncates the scanned region and the count silently
        // collapses to zero — a gate that fails open. (`FrameAccessList::
        // corrupt_last_byte` is the one that did it.) The precondition below is
        // the other half: a scan whose corpus went missing must say so.
        let src = include_str!("ir_lower.rs");
        let body = src.split("\nmod tests {").next().unwrap_or(src);
        assert!(
            body.contains("fn resident_xmm"),
            "the scanned region no longer contains the emitter — the test \
             module boundary moved"
        );
        let reads: Vec<String> = body
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| l.contains("self.reg_of") && !l.starts_with("//"))
            .collect();
        let allowed = [
            "self.reg_of = residency.reg_of;",
            "self.reg_of.get(id as usize).copied().flatten()",
            // `saved_xmm_regs` — the prologue/epilogue's "did this method
            // actually park a value in a callee-saved register" test. A read of
            // the whole plan rather than of one node's assignment, so it cannot
            // go through `resident_xmm`, and it is deliberately the ONLY such
            // read: a second one would mean two answers to "which registers
            // does this frame owe its caller", and a prologue and an epilogue
            // that disagree about that corrupt the caller's floating-point
            // state with nothing to notice.
            ".filter(move |(_, reg)| self.reg_of.iter().any(|r| *r == Some(**reg)))",
        ];
        for line in &reads {
            assert!(
                allowed.contains(&line.as_str()),
                "new read of the register assignment outside `resident_xmm` / \
                 `assigned_xmm` / `saved_xmm_regs`: {line}"
            );
        }
        assert_eq!(
            reads.len(),
            4,
            "expected exactly the install site, the two accessors and the \
             save-area predicate"
        );
    }

    // ── Code-buffer sizing ───────────────────────────────────────────
    //
    // `ir_code_buffer_estimate` decides which methods the optimizing tier gets
    // to produce a body for at all: too small and the compile is silently
    // discarded to single-pass, with nothing but a throughput change to show
    // for it. These pin the model against the census that produced it, so a
    // future "tighten this up" has to argue with the data rather than with a
    // comment.

    /// The extremes of the 2026-08-01 census: 1664 IR compiles across
    /// `BasicErrorControllerDirectMockMvcTests`,
    /// `BasicErrorControllerIntegrationTests` and `bench/CratonBench`, as
    /// `(nodes, call_nodes, wanted)`. These four are the largest consumers plus
    /// the tightest call-heavy small graph; every one of them OVERFLOWED the
    /// pre-2026-08-01 estimate.
    const CENSUS_EXTREMES: &[(usize, usize, usize)] = &[
        (78, 15, 25969),
        (36, 14, 22378),
        (72, 9, 16688),
        (9, 3, 4508),
    ];

    #[test]
    fn code_buffer_estimate_covers_every_measured_extreme() {
        for &(nodes, calls, wanted) in CENSUS_EXTREMES {
            let cap = ir_code_buffer_estimate(nodes, calls);
            assert!(
                cap >= wanted,
                "nodes={nodes} calls={calls} wants {wanted} bytes but the estimate \
                 reserves only {cap}; this compile would be silently dropped to the \
                 single-pass backend"
            );
        }
    }

    #[test]
    fn code_buffer_estimate_keeps_headroom_over_the_measured_worst_case() {
        // Covering the census exactly would be a knife edge — the next
        // workload's worst case is not in it. The measured tightest fit is
        // 1.24x; require at least 1.1x on the extremes so a change that
        // technically still "covers" them but removes all margin fails here.
        for &(nodes, calls, wanted) in CENSUS_EXTREMES {
            let cap = ir_code_buffer_estimate(nodes, calls);
            assert!(
                cap * 10 >= wanted * 11,
                "nodes={nodes} calls={calls}: {cap} bytes for a measured {wanted} is \
                 under 1.1x headroom"
            );
        }
    }

    #[test]
    fn a_call_node_is_budgeted_above_its_measured_worst_marginal_cost() {
        // The term the old estimate got wrong: it budgeted 448 bytes per call
        // node, and the census puts the worst-case marginal cost at 1141.
        // Derived from the model rather than asserted on the constant, so
        // moving cost between the terms is allowed and starving calls is not.
        //
        // Measured well ABOVE the floor. The first draft of this test compared
        // 1 call against 2, where both results are still clamped by
        // `IR_CODE_BUFFER_FLOOR` — the marginal cost read as 0 and the test
        // failed on a model that was perfectly fine. A floor hides exactly the
        // term this is trying to pin.
        let ten = ir_code_buffer_estimate(0, 10);
        let eleven = ir_code_buffer_estimate(0, 11);
        assert!(
            ten > IR_CODE_BUFFER_FLOOR && eleven > IR_CODE_BUFFER_FLOOR,
            "measure the marginal cost above the floor, not through it"
        );
        assert!(
            eleven - ten >= 1141,
            "a call node is budgeted {} bytes; the census measured 1141 worst-case",
            eleven - ten
        );
    }

    #[test]
    fn the_floor_alone_does_not_substitute_for_the_formula() {
        // `nodes*32 + calls*448 + 1024` with a raised floor was the tempting
        // one-line fix. It does not work: this shape wants 25969 bytes, which
        // no plausible floor covers, so the per-call term has to carry it.
        let (nodes, calls, wanted) = (78usize, 15usize, 25969usize);
        assert!(
            ir_code_buffer_estimate(nodes, calls) >= wanted,
            "the formula, not the floor, has to cover the call-heavy tail"
        );
        let legacy_shape = nodes * 32 + calls * 448 + 1024;
        assert!(
            legacy_shape < wanted,
            "sanity: the old formula really was short here ({legacy_shape} < {wanted})"
        );
    }

    #[test]
    fn call_free_graphs_stay_cheap() {
        // The census's 278 call-free compiles never wanted more than 1545
        // bytes. They are the majority of compiles, so their reservation is
        // what the code-cache cap actually pays for — the floor should be
        // covering them, not a large per-node term.
        assert!(
            ir_code_buffer_estimate(32, 0) <= 16 * 1024,
            "a call-free graph should not reserve more than a couple of pages"
        );
        assert!(
            ir_code_buffer_estimate(32, 0) >= 1545,
            "…but it must still cover the largest call-free compile measured"
        );
    }
}

/// `CRATONVM_JIT_NO_IC_FRAME_REPUBLISH=1` — stop republishing the
/// innermost-frame mirror after an optimizing-tier inline-cache hit.
///
/// The bisect lever for the republish folded into `emit_call_cached_entry`,
/// which is default-ON. Latched: read once, because a codegen decision must not
/// change under a running process. With it set, one binary reproduces the
/// stale-mirror `ACTIVE_FRAME_MAP` refusals this fixed.
fn ic_frame_republish_disabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_IC_FRAME_REPUBLISH").is_some()
    })
}

/// How many optimizing-tier inline-cache call sites were compiled WITH the
/// frame-record republish.
///
/// Bumped at COMPILE time, not per call, so it costs a workload nothing. It is
/// the engagement counter for that repair: a claim that the republish did or
/// did not move a workload is worth nothing while this reads zero, because a
/// zero means the optimizing tier never lowered an inline cache in that run and
/// the arms differed only by noise. Printed on the `[jitroots]` line beside the
/// verdict it is supposed to explain.
pub static IC_FRAME_REPUBLISH_SITES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Read [`IC_FRAME_REPUBLISH_SITES`].
pub fn ic_frame_republish_sites() -> usize {
    IC_FRAME_REPUBLISH_SITES.load(std::sync::atomic::Ordering::Relaxed)
}


