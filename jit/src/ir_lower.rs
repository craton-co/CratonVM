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
    Graph, InlineScopeTable, IrType, MemKind, NodeId, Op, SafepointSnapshot, MAX_INLINE_SCOPE_DEPTH,
    NO_NODE,
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
use cratonvm_types::{ARRAY_LENGTH_OFFSET, FIELD_CELL_PAYLOAD32_OFFSET, HEADER_SIZE, SLOT_SIZE};

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
    /// Compact-layout/TLAB-aware object allocation helper. Live `Op::New`
    /// nodes use the same shared runtime-lowering stub as the baseline tier.
    new_object: usize,
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
    /// `bytecode_pc → (packed byte offset from the object body, is_reference,
    /// descriptor tag)`. Empty ⇒ every `Op::Load` takes the checked helper.
    ///
    /// Present for the same reason the single-pass backend carries it: routing
    /// every field read through `jit_getfield` costs a boundary note, a region
    /// walk and a 16-byte atomic cell read on one of the hottest operations a
    /// JIT emits. The single-pass backend measured that at 4.7x on bintrees-16
    /// when the hardening first landed; the IR tier still paid it, which is why
    /// a forced-C2 bt18 ran 1.85x slower than the C1 body it replaced.
    compact_fields: HashMap<usize, (u32, bool, u8)>,
    /// Address of the GC's published `JIT_REGION_BOUNDS` table, for the guarded
    /// receiver check. Zero ⇒ no inline field read (the guard cannot be
    /// emitted, so the helper stays).
    region_bounds_addr: usize,
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
    /// locals region (locals + context + sp-id).
    first_spill: i32,
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
        sr_map: Option<&'a ScalarReplacementMap>,
        direct_calls: &'a HashMap<usize, (usize, bool)>,
        ic_slots: &'a HashMap<usize, (usize, usize)>,
        compact_fields: &HashMap<usize, (u32, bool, u8)>,
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

        // Frame layout (rbp downward): locals, [context slot], spills, [args
        // staging], 16-byte stack-arg reserve, 32-byte shadow. Reserve slots for
        // locals + one per COLOUR + shadow. The 16-byte tail above the shadow
        // region holds in-frame stack args for any helper called without
        // `emit_stack_arg_setup`; see the matching comment in `x64.rs`
        // (Compiler::new) for the worst-case 6-arg `jit_invoke_virtual_mic` site.
        let locals_size = (num_locals as i32) * 8;
        let context_size = if needs_context { 8 } else { 0 };
        // Four extra reserved slots: the safepoint id, the cached
        // `*mut JvmThread`, the shadow stack's base `top` for this push, and
        // the shadow `top` watermark captured at method entry.
        let sp_id_size = 8i32 * 4;
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
        debug_assert_eq!(
            frame_size,
            ((locals_size
                + context_size
                + sp_id_size
                + spill_size
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
        let first_spill = (base + 4) * 8;
        let args_stage_top_off = frame_size - shadow - stack_arg_reserve;
        let spill_cap_off = frame_size - shadow - stack_arg_reserve - args_stage_size;

        Lowerer {
            graph,
            schedule,
            buf,
            node_slot: vec![None; graph.nodes.len()],
            unallocated_slot_use: std::cell::Cell::new(false),
            latched_bailout: std::cell::RefCell::new(None),
            slot_plan,
            spill_high_water: first_spill,
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
            putfield_int: helpers.putfield_int,
            new_object: helpers.new_object,
            safepoint_flag_addr: helpers.safepoint_flag_addr,
            safepoint_slow_path: helpers.safepoint_slow_path,
            needs_context,
            context_slot_off,
            sp_id_slot_off,
            shadow_thread_slot_off,
            ref_param_homes,
            shadow_savebase_slot_off,
            shadow_savetop_slot_off,
            get_current_thread: helpers.get_current_thread,
            shadow_off_in_thread: helpers.shadow_stack_offset_in_thread as i32,
            pending_shadow: Vec::new(),
            thread_fetch_span: None,
            shadow_pushed_any: false,
            compact_fields: compact_fields.clone(),
            region_bounds_addr: helpers.region_bounds_addr,
            shadow_pushes: 0,
            shadow_reloads: 0,
            locals_size,
            first_spill,
            oop_maps: Vec::new(),
            defined_nodes: vec![false; graph.nodes.len()],
            next_sp_id: 1,
            args_stage_top_off,
            spill_cap_off,
            call_exc_patches: Vec::new(),
            self_call_patches: Vec::new(),
            direct_calls,
            ic_slots,
            invoke_virtual_mic: helpers.invoke_virtual_mic,
            frame_record: helpers.frame_record,
            service_callee_deopt: helpers.service_callee_deopt,
            branch_hints,
            sr_map,
            inline_scopes,
        }
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
        let color = match self.slot_plan.node_color.get(id as usize).copied().flatten() {
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
    fn emit_mov_tls_disp32_rbp(&mut self, disp32: u32) {
        self.buf.emit_byte(crate::x64::inline_rbp_tls_segment_prefix());
        self.buf.emit_byte(0x48); // REX.W
        self.buf.emit_byte(0x89); // MOV r/m64, r64
        self.buf.emit_byte(0x2C); // ModRM: reg=RBP, r/m=SIB
        self.buf.emit_byte(0x25); // SIB: [disp32] absolute
        self.buf.emit(&disp32.to_le_bytes());
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
        self.emit_mov_reg_imm64(RAX, self.get_current_thread as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
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
            self.buf.emit(&(-self.shadow_savetop_slot_off).to_le_bytes());
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
    /// Overwriting with `0x90` rather than shifting the body keeps every
    /// already-recorded offset — branch patches, deopt points, oop-map native
    /// pcs — valid, which is why this is an erase and not a removal.
    fn finish_lazy_thread_fetch(&mut self) {
        if self.shadow_pushed_any {
            return;
        }
        if let Some((start, end)) = self.thread_fetch_span.take() {
            for off in start..end {
                let _ = self.buf.try_patch_byte(off, 0x90);
            }
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
        self.buf.emit(&(-self.shadow_savebase_slot_off).to_le_bytes());
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
            self.buf.emit(&(-self.shadow_savebase_slot_off).to_le_bytes());
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
    /// region. Everything else — null, unaligned, out-of-heap, a legacy
    /// (non-compact) instance of a compact class, or a width this arm does not
    /// emit — branches to the helper, whose NPE / `i64::MIN` semantics are
    /// unchanged. The one simplification against the single-pass version: the
    /// legacy-layout receiver takes the helper rather than a second inline
    /// path.
    fn emit_inline_compact_getfield(
        &mut self,
        node_pc: Option<usize>,
        node_ty: IrType,
        base: NodeId,
        field_index: i64,
        slot: i32,
    ) -> bool {
        if self.getfield == 0 {
            return false;
        }
        let Some(pc) = node_pc else {
            return false;
        };
        let Some(&(c_off, c_is_ref, type_tag)) = self.compact_fields.get(&pc) else {
            return false;
        };
        if crate::x64::narrow_oops_block_inline_fields() {
            return false;
        }
        let raw_mode = crate::x64::inline_getfield_enabled();
        let guarded = crate::x64::guarded_inline_getfield_enabled() && self.region_bounds_addr != 0;
        if !raw_mode && !guarded {
            return false;
        }
        // The node type and the resolved descriptor must agree. They can only
        // disagree through a resolver that fabricated a compact slot — the
        // WildFly Host Controller SIGSEGV — and the consequence of trusting it
        // here would be a 32-bit sign-extended load of half a pointer.
        let ref_node = node_ty == IrType::Ref;
        let ref_tag = matches!(type_tag, b'L' | b'[');
        if ref_node != ref_tag || ref_tag != c_is_ref {
            return false;
        }
        if !ref_node && !matches!(type_tag, b'I' | b'Z' | b'B' | b'C' | b'S') {
            return false;
        }

        let cell_off = (HEADER_SIZE + c_off as usize) as i32;
        let mut slow: Vec<usize> = Vec::new();

        self.load_to_rax(self.slot_of(base));
        // 1. null → slow (the helper raises the NPE).
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        slow.push(self.emit_jcc_rel32(0x84)); // JZ
        if guarded && !raw_mode {
            // 2. alignment: the low three bits must be clear.
            self.buf.emit(&[0x48, 0x89, 0xC1]); // MOV RCX, RAX
            self.buf.emit(&[0x48, 0x83, 0xE1, 0x07]); // AND RCX, 7
            slow.push(self.emit_jcc_rel32(0x85)); // JNZ
                                                  // 3. containment in one of the three published regions.
                                                  //    RDX = &JIT_REGION_BOUNDS = [b0, e0, b1, e1, b2, e2].
            self.emit_mov_reg_imm64(RDX, self.region_bounds_addr as u64);
            self.emit_cmp_rax_mem_rdx(0);
            let below_b0 = self.emit_jcc_rel32(0x82); // JB → try region 1
            self.emit_cmp_rax_mem_rdx(8);
            let ok0 = self.emit_jcc_rel32(0x82); // JB → inside region 0
            self.patch_rel32_to_here(below_b0);
            self.emit_cmp_rax_mem_rdx(16);
            let below_b1 = self.emit_jcc_rel32(0x82); // JB → try region 2
            self.emit_cmp_rax_mem_rdx(24);
            let ok1 = self.emit_jcc_rel32(0x82); // JB → inside region 1
            self.patch_rel32_to_here(below_b1);
            self.emit_cmp_rax_mem_rdx(32);
            slow.push(self.emit_jcc_rel32(0x82)); // JB → slow
            self.emit_cmp_rax_mem_rdx(40);
            slow.push(self.emit_jcc_rel32(0x83)); // JAE → slow
            self.patch_rel32_to_here(ok0);
            self.patch_rel32_to_here(ok1);
        }
        // 4. per-OBJECT compactness. A class with a registered compact layout
        //    can still have legacy 16-byte-cell instances (an allocation whose
        //    `num_fields` disagrees with the layout falls back to the uniform
        //    plan), and reading one at the packed offset yields a mangled
        //    {tag, half-pointer} word.
        self.buf.emit(&[0xF6, 0x80]); // TEST byte [RAX + disp32], imm8
        self.buf
            .emit(&(cratonvm_types::GC_FLAGS_OFFSET as i32).to_le_bytes());
        self.buf.emit_byte(cratonvm_types::GC_FLAG_COMPACT);
        slow.push(self.emit_jcc_rel32(0x84)); // JZ → slow (legacy instance)

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
                _ => self.buf.emit(&[0x48, 0x63, 0x80]),          // MOVSXD RAX, dword
            }
            self.buf.emit(&cell_off.to_le_bytes());
        }
        self.buf.emit_byte(0xE9); // JMP rel32 → done
        let done_patch = self.buf.pos();
        self.buf.emit(&[0; 4]);

        // --- slow path: the checked helper, byte-identical to the arm this
        //     replaces, including the sentinel bail. ---
        for p in slow {
            self.patch_rel32_to_here(p);
        }
        self.load_reg_from_frame(CALL_ARG_REGS[0], self.context_slot_off);
        self.load_reg_from_frame(CALL_ARG_REGS[1], self.slot_of(base));
        self.emit_mov_reg_imm64(CALL_ARG_REGS[2], field_index as u64);
        self.emit_mov_reg_imm64(RAX, self.getfield as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        self.emit_mov_reg_imm64(R10, i64::MIN as u64);
        self.buf.emit(&[0x4C, 0x39, 0xD0]); // CMP RAX, R10
        self.buf.emit(&[0x0F, 0x84]); // JE rel32 → shared bail stub
        let exc_patch = self.buf.pos();
        self.buf.emit(&[0; 4]);
        self.call_exc_patches.push(exc_patch);

        self.patch_rel32_to_here(done_patch);
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
        self.buf.emit(&(-self.shadow_savebase_slot_off).to_le_bytes());
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
        self.buf.emit(&(-self.shadow_savebase_slot_off).to_le_bytes());
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
                matches!(self.graph.nodes[id].op, Op::Phi)
                    && self.graph.nodes[id].ty == IrType::Ref
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
        self.emit_mov_reg_imm64(R11, self.safepoint_flag_addr as u64);
        self.buf.emit(&[0x41, 0xF6, 0x03, 0xFF]); // TEST byte ptr [R11], 0xff
        self.buf.emit(&[0x0F, 0x84]); // JZ .clear
        let clear_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.emit_mov_reg_imm64(RAX, self.safepoint_slow_path as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        let rel = self.buf.pos() as i32 - (clear_patch as i32 + 4);
        // IR safepoint poll -- tolerated on an overflowed buffer; see
        // `Self::patch_or_bail` / `patch_rel32_to_here`.
        Self::patch_or_bail(&mut self.buf, clear_patch, rel);
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

    /// `MOV qword [rbp - off], 0` (mod=10 disp32, /0).
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
        self.buf.emit(&(-self.shadow_savetop_slot_off).to_le_bytes());
        self.buf.emit(&[0x4D, 0x89, 0x9A]);
        self.buf.emit(&ss_top.to_le_bytes());
        self.patch_rel32_to_here(skip);
    }

    fn emit_epilogue(&mut self) {
        self.emit_shadow_savetop_restore();
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
        // ir_lower self-call stack-sample -- tolerated on an overflowed buffer; see
        // `Self::patch_or_bail` / `patch_rel32_to_here`.
        Self::patch_or_bail(&mut self.buf, fast_skip_patch, rel);

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
        self.call_exc_patches.push(exc_patch);
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
    /// bound, so no receiver type check is needed — exactly why single-pass may
    /// bind them directly too.
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
    /// argument register on either ABI. The caller must have verified that
    /// `num_args + needs_context <= ENTRY_ABI_REGS.len()`, since there is no
    /// stack-argument path here.
    ///
    /// SAFETY: `entry` is a code address produced by this JIT for the resolved
    /// callee; `lib.rs` records it in `CompiledMethod::_direct_callee_entries`,
    /// which both keeps the callee's buffer alive (`_direct_callee_roots`) and
    /// puts this method into the callee's invalidation closure, so the baked
    /// address can never outlive the code it points at.
    fn emit_direct_cross_call(
        &mut self,
        inputs: &[NodeId],
        slot: i32,
        num_args: usize,
        entry: usize,
        callee_needs_ctx: bool,
        info_ptr: usize,
        ty: IrType,
    ) {
        for i in 0..num_args {
            let arg = inputs[2 + i];
            self.load_to_rax(self.slot_of(arg));
            self.store_rax(self.args_stage_top_off - (i as i32) * 8);
        }
        let base = if callee_needs_ctx {
            self.load_reg_from_frame(ENTRY_ABI_REGS[0], self.context_slot_off);
            1
        } else {
            0
        };
        for i in 0..num_args {
            let arg = inputs[2 + i];
            self.load_reg_from_frame(ENTRY_ABI_REGS[base + i], self.slot_of(arg));
        }
        // MOV RAX, entry ; CALL RAX.
        self.emit_mov_reg_imm64(RAX, entry as u64);
        self.buf.emit(&[0xFF, 0xD0]);
        self.emit_inline_callee_deopt_service(info_ptr, num_args);
        self.emit_call_return_check(slot, ty);
    }

    /// Service an exceptional return from an inline cached compiled callee.
    /// The arguments already live in the fixed staging area, so this preserves
    /// the callee identity and incoming locals until its own handler can run.
    fn emit_inline_callee_deopt_service(&mut self, info_ptr: usize, num_args: usize) {
        if self.service_callee_deopt == 0 {
            return;
        }
        self.emit_mov_reg_imm64(R10, i64::MIN as u64);
        self.buf.emit(&[0x4C, 0x39, 0xD0]); // CMP RAX, R10
        self.buf.emit(&[0x0F, 0x85]); // JNE .done
        let skip = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
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
            self.load_reg_from_frame(ENTRY_ABI_REGS[base + i], self.slot_of(arg));
        }
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
        // The MIC/PIC hit service needs the exact outgoing Java arguments too.
        for i in 0..num_args {
            let arg = inputs[2 + i];
            self.load_to_rax(self.slot_of(arg));
            self.store_rax(self.args_stage_top_off - (i as i32) * 8);
        }

        // Receiver = arg0. Load it and its class id ONCE for the whole cascade.
        self.load_to_rax(self.slot_of(inputs[2]));
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        slow_patches.push(self.emit_jcc_rel32(0x84)); // JZ .slow
        // Array-receiver guard — `ObjectHeader.class_id` (offset 0) holds a
        // reference array's COMPONENT class id, so `Foo[]` and `Foo` share the
        // guard word every cached entry below is compared against. Without the
        // `kind` check a `Foo[]` receiver is dispatched into `Foo`'s method
        // body. See the matching guard in `x64.rs`'s single-pass cascade.
        //   CMP BYTE [RAX + OBJECT_KIND_OFFSET], ObjectKind::Object
        self.buf.emit(&[
            0x80,
            0x78,
            cratonvm_types::OBJECT_KIND_OFFSET as u8,
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
        let mic_noctx = self.emit_jcc_rel32(0x84); // JE .mic_noctx
        self.emit_ic_abi_marshal(inputs, num_args, true);
        let mic_call = self.emit_jmp_rel32();
        self.patch_rel32_to_here(mic_noctx);
        self.emit_ic_abi_marshal(inputs, num_args, false);
        self.patch_rel32_to_here(mic_call);
        self.emit_call_cached_entry(JitMICSlot::CACHED_ENTRY_PTR_OFFSET as u8);
        self.emit_inline_callee_deopt_service(info_ptr, num_args);
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
            let noctx = self.emit_jcc_rel32(0x84); // JE .entry_noctx
            self.emit_ic_abi_marshal(inputs, num_args, true);
            let call = self.emit_jmp_rel32();
            self.patch_rel32_to_here(noctx);
            self.emit_ic_abi_marshal(inputs, num_args, false);
            self.patch_rel32_to_here(call);
            self.emit_call_cached_entry(JitPICSlot::ENTRY_PTR_OFFSETS[i] as u8);
            self.emit_inline_callee_deopt_service(info_ptr, num_args);
            done_patches.push(self.emit_jmp_rel32());
        }
        debug_assert!(next_entry.is_none());

        // ── Shared compact hashed/vtable stub ─────────────────────────────
        for p in slow_patches {
            self.patch_rel32_to_here(p);
        }
        let arg_offsets: Vec<i32> = (0..num_args).map(|i| self.slot_of(inputs[2 + i])).collect();
        done_patches.extend(crate::runtime_lowering::emit_hashed_vtable_stub(
            &mut self.buf,
            pic,
            self.context_slot_off,
            &arg_offsets,
            self.frame_record,
            self.service_callee_deopt,
            info_ptr,
        ));

        // ── Slow path: the resolving + cache-populating helper ────────────
        // Args 1..4 use the same ABI as `jit_invoke_dispatch`, so the staging
        // marshalling is identical to the generic path below.
        for i in 0..num_args {
            let arg = inputs[2 + i];
            self.load_to_rax(self.slot_of(arg));
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
            self.call_exc_patches.push(patch);
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
            self.call_exc_patches.push(patch);
        }
        self.store_rax(slot);
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
            Op::New {
                class_id,
                num_fields,
            } => {
                let slot = self.alloc_slot(id);
                crate::runtime_lowering::emit_new_object_stub(
                    &mut self.buf,
                    self.context_slot_off,
                    self.new_object,
                    *class_id,
                    *num_fields,
                    self.frame_record,
                );

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
                self.call_exc_patches.push(exception_patch);
                let allocated = self.buf.pos();
                let rel = allocated as i32 - (allocated_patch as i32 + 4);
                // allocation success -- tolerated on an overflowed buffer; see
                // `Self::patch_or_bail` / `patch_rel32_to_here`.
                Self::patch_or_bail(&mut self.buf, allocated_patch, rel);
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
            // `Op::Load(MemKind::Int)` for the int-category fields and
            // `Op::Load(MemKind::Ref)` for reference fields; both read through
            // the checked `jit_getfield` helper, which returns the int payload
            // or the raw pointer according to the receiver's registered layout.
            // The inline fallback below is int-only and layout-naive, and
            // `lower_inner` refuses any graph that would need it for a
            // reference load. inputs = [ctrl, mem, base, offset] where `offset`
            // is a `Const(field_index)`.
            Op::Load(_) => {
                let slot = self.alloc_slot(id);
                let base = node.inputs[2];
                let offset_node = node.inputs[3];
                let field_index = match self.graph.nodes[offset_node as usize].op {
                    Op::Const(v) => v,
                    _ => 0,
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
            // the heap write: a null receiver DEOPTS (the interpreter then
            // re-executes this putfield and throws NullPointerException), else
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
                let bci = node.bytecode_pc.unwrap_or(0);
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
                    self.load_to_rax(self.slot_of(base));
                    self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                    self.emit_deopt_if_zero(bci, DeoptReason::NullCheck);
                    // jit_putfield_int(obj_ptr, field_index, val) — no context.
                    // Receiver first: `load_reg_from_frame` into arg0 would be
                    // clobbered by nothing here, but keep the order arg0..arg2
                    // so a future stack-arg spill sees a conventional sequence.
                    self.load_reg_from_frame(CALL_ARG_REGS[0], self.slot_of(base));
                    self.emit_mov_reg_imm64(CALL_ARG_REGS[1], field_index as i64 as u64);
                    self.load_reg_from_frame(CALL_ARG_REGS[2], self.slot_of(value));
                    self.emit_mov_reg_imm64(RAX, self.putfield_int as u64);
                    self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                    return;
                }
                // Receiver → RAX, value → RCX.
                self.load_to_rax(self.slot_of(base));
                self.load_to_rcx(self.slot_of(value));
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
                // `emit_direct_cross_call`. There is no stack-argument path, so a
                // site whose args do not fit the entry ABI register file falls
                // through to the (unchanged) helper dispatch below rather than
                // being dropped. `lower_data_node` has no post-`match` code, so
                // an early `return` here fully handles the node.
                if let Some(&(entry, callee_needs_ctx)) =
                    node.bytecode_pc.and_then(|pc| self.direct_calls.get(&pc))
                {
                    if entry != 0
                        && num_args + usize::from(callee_needs_ctx) <= ENTRY_ABI_REGS.len()
                    {
                        self.emit_direct_cross_call(
                            &node.inputs,
                            slot,
                            num_args,
                            entry,
                            callee_needs_ctx,
                            *info_ptr,
                            node.ty,
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
                    if let Some(&(mic, pic)) = node.bytecode_pc.and_then(|pc| self.ic_slots.get(&pc)) {
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
                // 4. Exception sentinel + result spill — shared with the direct
                //    cross-method call path; see `emit_call_return_check`.
                self.emit_call_return_check(slot, node.ty);
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
                    None if matches!(node.op, Op::Dead) => {
                        FrameValue::MaterializationRequired(EliminatedValue::new(
                            node_id,
                            EliminationCause::Unclassified,
                        ))
                    }
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
        // Shares the method epilogue so the shadow `top` watermark is restored
        // here too — this is the path a callee's `i64::MIN` exception/deopt
        // sentinel takes, skipping the call site's matching shadow reload.
        self.emit_epilogue();
        let patches = std::mem::take(&mut self.call_exc_patches);
        for p in patches {
            let rel = stub_off as i32 - (p as i32 + 4);
            if !Self::patch_or_bail(&mut self.buf, p, rel) {
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
    /// one. An empty caller frame is *silently wrong*: every resume sink maps a
    /// missing slot to `Value::Int(0)`, so the interpreter would resume the
    /// caller with all-zero locals and no error anywhere. `Unsupported` makes
    /// the whole chain fail [`crate::deopt::frame_state_is_resumable`], so the
    /// deopt takes the safe whole-method re-run instead. That predicate walks
    /// the caller chain precisely so this cannot be resumed as if clean.
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
                    if sr.objects.contains_key(&vnode) {
                        if dbg {
                            eprintln!("[DBG_SCALAR_DEOPT] bail new {new_id}: field {i} is nested virtual (node {vnode})");
                        }
                        // nested virtual — deferred (v1 emits no nested graphs)
                        return Self::eliminated_object(
                            new_id,
                            info,
                            EliminationCause::NestedVirtualObject,
                        );
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
        if matches!(n.op, Op::New { .. }) {
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
/// the four reserved bookkeeping slots, the spill reservation, the outgoing
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
    // safepoint id, cached thread pointer, shadow save-base, shadow save-top.
    let bookkeeping = 8usize * 4;
    let spills = spill_slots.saturating_mul(8);
    let args_stage = needs.max_call_args.saturating_mul(8);
    let shadow = 32usize;
    let stack_arg_reserve = 16usize;
    let total = locals
        .saturating_add(context)
        .saturating_add(bookkeeping)
        .saturating_add(spills)
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
/// emit nothing — are deliberately absent, because a value read from one of
/// them has no location either.
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
            | Op::New { .. }
            | Op::Call { .. }
            | Op::LambdaIntToDouble
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
    let mut seq = 0usize;
    for block in &schedule.blocks {
        for &node_id in &block.nodes {
            if let Some(cell) = emit_index.get_mut(node_id as usize) {
                *cell = Some(seq);
            }
            seq += 1;
        }
        if let Some(term) = block.terminator {
            if let Some(cell) = emit_index.get_mut(term as usize) {
                *cell = Some(seq);
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
                         use by n{user} ({:?}) at position {use_seq}",
                        def.op, node.op
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
    bits.get(i / 64).is_some_and(|w| w & (1u64 << (i % 64)) != 0)
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
        if node.ty == IrType::Ref && matches!(node.op, Op::Call { .. }) {
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
fn verify_slot_colouring(
    graph: &Graph,
    schedule: &Schedule,
    plan: &SlotPlan,
) -> CompileResult<()> {
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
                    format!("slot {color} is shared by {} values, one pinned", bucket.len()),
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

/// Funnel every refusal in this file through one place: count it, then return
/// the `None` the public entry points have always returned, which is the
/// caller's signal to run the method in a lower tier. A compiler refusal must
/// never terminate the VM.
fn refuse(bailout: Bailout) -> Option<CompiledMethod> {
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_IR_BAILOUT").is_some() {
        eprintln!("[ir-bailout] {bailout}");
    }
    record_bailout(&bailout);
    None
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
    let no_compact: HashMap<usize, (u32, bool, u8)> = HashMap::new();
    lower_inner(
        graph,
        schedule,
        num_params,
        num_locals,
        helpers,
        &empty,
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
    let no_compact: HashMap<usize, (u32, bool, u8)> = HashMap::new();
    lower_inner(
        graph,
        schedule,
        num_params,
        num_locals,
        helpers,
        branch_hints,
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
    let no_compact: HashMap<usize, (u32, bool, u8)> = HashMap::new();
    lower_inner(
        graph,
        schedule,
        num_params,
        num_locals,
        helpers,
        &empty,
        sr_map,
        &no_direct,
        &no_ic,
        &no_compact,
    )
}

/// Shared lowering body: profile-guided branch hints, the optional
/// guard-surviving scalar-replacement map, and the two per-call-site lowering
/// tables all flow in here. `pub(crate)` so the production compile path
/// (`lib.rs`) can supply all of them at once.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lower_inner(
    graph: &Graph,
    schedule: &Schedule,
    num_params: usize,
    num_locals: usize,
    helpers: &JitRuntimeHelpers,
    branch_hints: &HashMap<usize, bool>,
    sr_map: Option<&ScalarReplacementMap>,
    // IR direct-call lowering: `pc → (callee_entry, callee_needs_context)` for
    // every statically-bound call site whose callee was eagerly compiled. Empty
    // ⇒ every `Op::Call` keeps the historical `jit_invoke_dispatch` lowering.
    direct_calls: &HashMap<usize, (usize, bool)>,
    // IR inline-cache lowering: `pc → (mic_slot_addr, pic_slot_addr)` for every
    // virtual / interface site served from an inline cache. Empty ⇒ every such
    // site keeps the historical helper dispatch.
    ic_slots: &HashMap<usize, (usize, usize)>,
    // Guarded inline field reads: `pc → (packed body offset, is_reference,
    // descriptor tag)` for every resolved compact instance field. Empty ⇒ every
    // `Op::Load` takes the checked helper, as it always did.
    compact_fields: &HashMap<usize, (u32, bool, u8)>,
) -> Option<CompiledMethod> {
    // A live object allocation is now supported by the common allocation
    // stub. A zero helper pointer is only possible in synthetic unit-test
    // tables; reject it instead of emitting a call through address zero.
    if helpers.new_object == 0
        && graph
            .nodes
            .iter()
            .any(|node| matches!(node.op, Op::New { .. }))
    {
        return None;
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
        let needs_getfield_helper = helpers.getfield == 0
            && graph.nodes.iter().any(|n| matches!(n.op, Op::Load(_)));
        let needs_putfield_helper = helpers.putfield_int == 0
            && graph.nodes.iter().any(|n| matches!(n.op, Op::Store(_)));
        if needs_getfield_helper || needs_putfield_helper {
            return None;
        }
    }
    // A REFERENCE field read has only one correct lowering: the helper. The
    // inline displacement fallback decodes a 16-byte int cell at
    // `HEADER_SIZE + index*SLOT_SIZE`, which for a reference slot yields the
    // discriminant word rather than the pointer — a fabricated address the
    // frame would then publish as a root. Refuse the graph outright rather
    // than emit it, independently of the compact-layout switch above.
    if helpers.getfield == 0
        && graph
            .nodes
            .iter()
            .any(|n| matches!(n.op, Op::Load(MemKind::Ref)))
    {
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
    let estimated_size = graph
        .nodes
        .len()
        .saturating_mul(32)
        .saturating_add(call_nodes.saturating_mul(448))
        .saturating_add(1024);
    let buf = ExecutableBuffer::new(estimated_size.max(4096))?;

    let mut lowerer = Lowerer::new(
        graph,
        schedule,
        buf,
        num_params,
        num_locals,
        &slot_plan,
        helpers,
        branch_hints,
        sr_map,
        direct_calls,
        ic_slots,
        compact_fields,
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
    if let Err(bailout) = lowerer.prealloc_phi_slots() {
        return refuse(bailout);
    }

    lowerer.emit_prologue();
    lowerer.emit_safepoint_poll();

    // Emit blocks in order
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
    let locals_size = lowerer.locals_size;
    let first_spill = lowerer.first_spill;
    let spill_cap_off = lowerer.spill_cap_off;
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

    let buf = lowerer.buf;
    // Soundness bail (jit-inlining-and-ir-calls). `ExecutableBuffer::emit` is
    // non-panicking: on capacity exhaustion it sets a sticky `overflowed` flag
    // and DROPS the write, so an under-estimated buffer yields a silently
    // truncated body — execution runs off the end of the emitted code. The
    // single-pass backend has always checked this; the IR lowerer never did,
    // which was harmless only while its per-node emission was tiny and bounded.
    // Inline caches (~250 bytes/site) plus the widened invoke budget make the
    // estimate materially harder, so check it here and let the caller fall back
    // to single-pass, exactly like the unallocated-slot latch above.
    if buf.overflowed() {
        return refuse(Bailout::new(BailoutReason::CodeBufferExhausted {
            needed: buf.pos(),
            capacity: estimated_size.max(4096),
        }));
    }
    let _code_size = buf.pos();

    let mut cm = CompiledMethod::new(buf);
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
    //     locals | [context] | sp-id | spills … | arg staging | argrsv | shadow
    //     ^0       ^locals    ^       ^first_spill            ^spill_cap_off
    //
    // `callee_saved_lo` names the start of the region the verifier must NOT
    // inspect. The IR prologue saves no callee-saved registers (it uses only
    // caller-saved scratch), so that region here is not a register save area
    // but the outgoing-argument staging, the stack-arg reserve and the ABI
    // shadow space — scratch this frame never resumes from, and full of dead
    // argument words. That is exactly the role the field plays on the reader
    // side (`band_slot_is_verifiable` skips everything at or above it).
    cm.sp_id_slot_off = sp_id_slot_off;
    cm.oop_maps = oop_maps;
    cm.osr_frame_size = frame_size;
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
        xmm_saved_lo: 0,
        xmm_saved_hi: 0,
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
                (t.starts_with(".expect(") || t.starts_with(".unwrap("))
                    && l.contains("patch")
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
        graph.exit = graph.add(
            Op::Return,
            IrType::Void,
            vec![ctrl, allocation],
            Some(3),
        );

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
        graph.exit = graph.add(
            Op::Return,
            IrType::Void,
            vec![ctrl, allocation],
            Some(3),
        );

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
    fn lower_virtual_call_with_ic(ic: &HashMap<usize, (usize, usize)>) -> Vec<u8> {
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

        let empty_hints: HashMap<usize, bool> = HashMap::new();
        let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
        let no_compact: HashMap<usize, (u32, bool, u8)> = HashMap::new();
        lower_inner(
            &graph,
            &schedule,
            2,
            2,
            &helpers,
            &empty_hints,
            None,
            &no_direct,
            ic,
            &no_compact,
        )
        .expect("virtual-call method must lower")
        .code_bytes()
        .to_vec()
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
        invoke_info.insert(2usize, (info as *const JitInvokeInfo as usize, 2usize, b'I'));
        builder.set_invoke_info(invoke_info);
        let graph = builder.build(&code, 6).expect("IR build of self-recursive call");
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
        let no_compact: HashMap<usize, (u32, bool, u8)> = HashMap::new();
        let cm = lower_inner(
            &graph,
            &schedule,
            2,
            2,
            &helpers,
            &empty_hints,
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

        // CMP BYTE [RAX + OBJECT_KIND_OFFSET], ObjectKind::Object
        let guard = [
            0x80u8,
            0x78,
            cratonvm_types::OBJECT_KIND_OFFSET as u8,
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
            &[0x4D, 0x8B, 0x5A, JitMICSlot::CACHED_ENTRY_PTR_OFFSET as u8]
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
                    &[0x4D, 0x8B, 0x5A, JitPICSlot::ENTRY_PTR_OFFSETS[i] as u8]
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
            uses: Default::default(),
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

    /// Every remaining bail in `frame_value_for_object` — not just the
    /// dominance one — must name the elimination rather than spell it
    /// `Undefined`. The nested-virtual bail additionally carries its own cause,
    /// so a compiler report can distinguish "escape analysis left me nothing to
    /// rebuild from" (fix the dominance gate) from "the recipe exists but v1
    /// emits no nested graphs" (implement nested virtual objects).
    #[test]
    fn scalar_deopt_nested_virtual_field_bails_with_its_own_cause() {
        let (mut g, mut sr_map, newo) = build_sr_deopt_graph(false, false);
        // Make field 0's value itself a scalar-replaced object: add a second
        // dead `Op::New` and register it in the map, then point the first
        // object's only field at it.
        let inner = g.add(Op::Dead, IrType::Ref, vec![], None);
        sr_map.objects.insert(
            inner,
            VirtualObjectInfo {
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
        assert_eq!(
            deopt_locals_at(&cm, 10)[1],
            FrameValue::MaterializationRequired(EliminatedValue::allocation(
                newo,
                7,
                EliminationCause::NestedVirtualObject,
            )),
            "a nested virtual field must bail with NestedVirtualObject, not \
             ScalarReplacedObject and not Undefined"
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
        };
        // The map covers [rbp-40]; deopt spells that word as StackSlotRef(-40).
        let verifier = DeoptVerifier::new().with_oop_map(0x40, OopCoverage::complete([40]));
        assert!(
            verifier
                .verify(std::slice::from_ref(&point(vec![FrameValue::StackSlotRef(
                    -40
                )])))
                .is_ok(),
            "a covered reference slot must pass"
        );
        let err = verifier
            .verify(std::slice::from_ref(&point(vec![FrameValue::StackSlotRef(
                -48,
            )])))
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
                    id: 12,
                    class_id: 3,
                    num_fields: 1,
                    field_values: vec![FrameValue::Int(0)],
                })],
                stack: vec![],
                monitors: vec![],
                caller: None,
            },
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
        let no_compact_fields: HashMap<usize, (u32, bool, u8)> = HashMap::new();
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
            None,
            &no_direct,
            &no_ic,
            &no_compact_fields,
        );

        // Unallocated is an error, not an offset.
        assert!(lowerer.slot_of_checked(a).is_err());
        assert!(lowerer.node_slot.iter().all(|slot| slot.is_none()));

        let off = lowerer.alloc_slot_checked(a).expect("first spill slot");
        assert!(off >= lowerer.first_spill, "{off} < {}", lowerer.first_spill);
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
        assert_eq!(latched.reason, BailoutReason::UnallocatedValue { node: sum });
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
        // stack-arg reserve and the ABI shadow space — not spill.
        assert!(after <= 160, "{after} bytes for a 4-slot working set");
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
}
