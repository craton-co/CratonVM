#[allow(unused_imports)]
use super::*;

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
// Built by `ea_ir_bridge::build_scalar_replacement_map_with_plans` from the
// escape-analysis result, once per EA round (so `class_id`/`num_fields`/
// `field_values` are captured before the `Op::New` is marked dead); `lib.rs`'s
// IR tier merges the rounds' maps and passes the result to
// `lower_inner_with_array_list` (tests use `lower_with_scalar_deopt`). `None`
// (descriptors off, or nothing replaced) preserves the exact prior behaviour —
// byte-identical default builds.

/// Per-scalar-replaced-object metadata the deopt producer needs to emit a
/// `FrameValue::VirtualObject`. Keyed (in [`ScalarReplacementMap`]) by the IR
/// `NodeId` of the eliminated `Op::New`, which is also used as the object's
/// stable [`crate::deopt::VirtualObjectState::id`] within a deopt frame.
#[derive(Clone)]
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
    /// The eliminated field stores' own node ids, parallel to `store_ctrls`.
    /// When a store's block IS the deopt block, strict dominance cannot answer
    /// and the id order within the block does — see
    /// `Lowerer::eliminated_node_precedes_deopt`. A shorter list than
    /// `store_ctrls` (a hand-built map) makes the same-block case refuse.
    pub store_nodes: Vec<NodeId>,
    /// Per eliminated store, parallel to `store_nodes`: the field index it
    /// writes and the IR value it writes (`None` value: a zero default).
    /// `None` entry, or a list shorter than `store_nodes` (a hand-built
    /// map): unknown, and the lowerer refuses any shape that needs it.
    pub store_fields: Vec<Option<(usize, Option<NodeId>)>>,
    /// `Some` for a LOOP-CARRIED object (escape analysis answered one of its
    /// loads with a value φ, `escape_analysis::ea_loop_carried_fields_enabled`):
    /// the value φs scalar replacement created, as `(field, φ node)`. Such an
    /// object is described at a deopt point by each field's reaching
    /// definition over the scheduled CFG (`Lowerer::loop_carried_field_values`),
    /// never by the store-dominance rules, which cannot see a store a loop's
    /// back edge carries. `Some(empty)` (the φs were never created) refuses
    /// every deopt point. `None` for every other object.
    pub loop_phis: Option<Vec<(usize, NodeId)>>,
    /// `Some(cond_node)` when this entry describes a null-guarded `Op::Phi`
    /// (`Phi(Merge, null, alloc)`): `cond_node` is the `IrType::Int` `0`/`1`
    /// φ indicating whether the object was allocated (`1`) or `null` (`0`).
    pub non_null_cond: Option<NodeId>,
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
pub(in crate::ir_lower) const DEOPT_ARG0: u8 = 1; // RCX
#[cfg(target_os = "windows")]
pub(in crate::ir_lower) const DEOPT_ARG1: u8 = 2; // RDX
#[cfg(not(target_os = "windows"))]
pub(in crate::ir_lower) const DEOPT_ARG0: u8 = 7; // RDI
#[cfg(not(target_os = "windows"))]
pub(in crate::ir_lower) const DEOPT_ARG1: u8 = 6; // RSI
#[cfg(target_os = "windows")]
pub(in crate::ir_lower) const DEOPT_ARG2: u8 = 8; // R8
#[cfg(not(target_os = "windows"))]
pub(in crate::ir_lower) const DEOPT_ARG2: u8 = 2; // RDX
                                                  // The 4th argument of `deopt::ir_deopt_entry_named` (interpreter round i2
                                                  // wave 60, lane L2): the compile id of the trapping body.
#[cfg(target_os = "windows")]
pub(in crate::ir_lower) const DEOPT_ARG3: u8 = 9; // R9
#[cfg(not(target_os = "windows"))]
pub(in crate::ir_lower) const DEOPT_ARG3: u8 = 1; // RCX

/// Bytes of caller shadow space reserved above `rsp` for the deopt stub's
/// `call ir_deopt_entry` (Win64 requires 32; harmless on SysV). Kept clear of
/// spill slots by `alloc_slot`. See its doc comment.
pub(in crate::ir_lower) const DEOPT_SHADOW_SPACE: i32 = 32;

// x86-64 register constants
#[allow(dead_code)]
pub(in crate::ir_lower) const RAX: u8 = 0;
#[allow(dead_code)]
pub(in crate::ir_lower) const RCX: u8 = 1;
#[allow(dead_code)]
pub(in crate::ir_lower) const RDX: u8 = 2;
pub(in crate::ir_lower) const RSP: u8 = 4;
#[allow(dead_code)]
pub(in crate::ir_lower) const R10: u8 = 10;
pub(in crate::ir_lower) const R11: u8 = 11;
// JIT round 24 B2: the arena cache's slot and key scratch (caller-saved, never
// a residency register).
pub(in crate::ir_lower) const R8: u8 = 8;
pub(in crate::ir_lower) const R9: u8 = 9;

// XMM scratch registers for the FP value tier (inc 30). Analogous to RAX/RCX:
// XMM0 holds the first operand / result, XMM1 the second operand / a mask.
pub(in crate::ir_lower) const XMM0: u8 = crate::regalloc::xmm_roles::IR_FP_SCRATCH[0];
pub(in crate::ir_lower) const XMM1: u8 = crate::regalloc::xmm_roles::IR_FP_SCRATCH[1];

/// The bytes of one `[RBP - disp]` access, without a heap allocation.
///
/// Seven is the longest form ([`enc_frame_load`]'s disp32 branch: REX + opcode
/// + ModRM + four displacement bytes).
#[derive(Clone, Copy)]
pub(in crate::ir_lower) struct FrameAccess {
    pub(in crate::ir_lower) bytes: [u8; 7],
    pub(in crate::ir_lower) len: usize,
}

impl FrameAccess {
    pub(in crate::ir_lower) fn new() -> FrameAccess {
        FrameAccess {
            bytes: [0; 7],
            len: 0,
        }
    }

    pub(in crate::ir_lower) fn push(&mut self, b: u8) {
        if let Some(cell) = self.bytes.get_mut(self.len) {
            *cell = b;
            self.len += 1;
        }
    }

    pub(in crate::ir_lower) fn as_slice(&self) -> &[u8] {
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
pub(in crate::ir_lower) struct FrameAccessList {
    pub(in crate::ir_lower) bytes: [u8; 32],
    pub(in crate::ir_lower) len: usize,
}

impl FrameAccessList {
    pub(in crate::ir_lower) fn new() -> FrameAccessList {
        FrameAccessList {
            bytes: [0; 32],
            len: 0,
        }
    }

    pub(in crate::ir_lower) fn push(&mut self, acc: &FrameAccess) -> Option<()> {
        self.push_bytes(acc.as_slice())
    }

    pub(in crate::ir_lower) fn push_bytes(&mut self, bytes: &[u8]) -> Option<()> {
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

    pub(in crate::ir_lower) fn as_slice(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }

    pub(in crate::ir_lower) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Make this tile's bytes wrong, on purpose.
    ///
    /// The forcing function for the byte-equality oracle: a check that has
    /// never been shown to fail is a check nobody knows the polarity of. See
    /// `an_injected_wrong_byte_is_caught_by_the_oracle`.
    #[cfg(test)]
    pub(in crate::ir_lower) fn corrupt_last_byte(&mut self) {
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
pub(in crate::ir_lower) fn enc_frame_load(reg: u8, offset: i32, out: &mut FrameAccess) {
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
pub(in crate::ir_lower) fn enc_frame_store(reg: u8, offset: i32, out: &mut FrameAccess) {
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
pub(in crate::ir_lower) const CALL_ARG_REGS: [u8; 4] = [1, 2, 8, 9]; // RCX, RDX, R8, R9
#[cfg(not(target_os = "windows"))]
pub(in crate::ir_lower) const CALL_ARG_REGS: [u8; 4] = [7, 6, 2, 1]; // RDI, RSI, RDX, RCX

// ABI-3-R / P3-7 — the claim above, tied to the helper census.
//
// Every `jit/src/x64/*.rs` emitter now carries `assert_helper_call_shape!` (or
// a hand-written `const _`, where that macro asserts the wrong shape) beside
// every helper-slot call. This tier has no `emit_call_absolute` call site to
// paste one at — its only mention of that function is in a doc comment — but it
// has the same hazard in a different shape: every one of the sites below stages
// arguments into `CALL_ARG_REGS[0]`..`[3]` and writes no stack slot at all.
// Nothing hand-writes a Win64 overflow word here the way
// `x64/op_invoke.rs`'s `invoke_virtual_mic` site does.
//
// Until this block, the sentence justifying that ("capped at 4 because the
// dispatch helper only ever takes 4", in the `ENTRY_ABI_REGS` comment below)
// was tied to nothing at all: a helper that grew a fifth integer argument would
// compile, and this tier would call it having loaded four registers, leaving
// the fifth whatever the emitted prologue happened to leave there.
const _: () = {
    let dispatch = cratonvm_jit_api::helpers_abi::helper_sig_of("invoke_dispatch");
    assert!(
        dispatch.arity == 4 && dispatch.float_args == 0 && dispatch.win64_stack_args() == 0,
        "`invoke_dispatch` no longer takes exactly four integer words. The IR \
         tier stages its arguments into CALL_ARG_REGS[0..=3] and writes no \
         stack slot, so a fifth word would be dropped silently and a \
         floating-point one would be read from the wrong register file",
    );
    assert!(
        CALL_ARG_REGS.len() >= 4,
        "this tier indexes CALL_ARG_REGS[3] unconditionally at sixteen sites",
    );
    assert!(
        cratonvm_jit_api::HELPERS_NEEDING_WIN64_STACK_ARGS == 1,
        "a second helper now overflows Win64's four argument registers. Only \
         `invoke_virtual_mic` ever did, and only the single-pass tier \
         hand-writes its two overflow slots ([RSP+32]/[RSP+40]); nothing in \
         this file writes an overflow slot at all, so a helper reached through \
         CALL_ARG_REGS that needs one is a silently truncated call",
    );
};

// Platform C-ABI integer argument registers for a JIT *method entry* — the ABI
// a compiled artifact's own prologue reads its incoming arguments from:
// `abi[0]` = the hidden VM context pointer when the callee `needs_context`,
// then one register per Java argument (receiver first for a non-static
// callee). This is the FULL integer argument register file, unlike
// `CALL_ARG_REGS` (capped at 4 because the dispatch helper only ever takes 4 —
// which is now asserted against the helper census, in the `const _` above,
// rather than only stated here).
// Mirrors x64.rs `ARG_REGS` and the inline list in `emit_self_recursive_call`.
#[cfg(target_os = "windows")]
pub(in crate::ir_lower) const ENTRY_ABI_REGS: &[u8] = &[1, 2, 8, 9]; // RCX, RDX, R8, R9
#[cfg(not(target_os = "windows"))]
pub(in crate::ir_lower) const ENTRY_ABI_REGS: &[u8] = &[7, 6, 2, 1, 8, 9]; // RDI, RSI, RDX, RCX, R8, R9

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
pub(in crate::ir_lower) const STACK_ARG_BASE: i32 = 16 + 32;
#[cfg(not(target_os = "windows"))]
pub(in crate::ir_lower) const STACK_ARG_BASE: i32 = 16;

/// JIT round 23 wave 5 (`CRATONVM_JIT_IR_DEOPT_NULL_CONST_AS_OBJECT`,
/// correctness, default ON, `=0` kill): a deopt frame state describes the
/// `Ref`-typed null constant as `FrameValue::Object(0)` (a null reference)
/// instead of `FrameValue::Int(0)`, which every resume sink rebuilt as the
/// `int` 0 in a reference slot. Read from the shared flag snapshot
/// (`JitFlags::deopt_null_const_as_object`), so no static of its own.
pub(crate) fn deopt_null_const_as_object() -> bool {
    cratonvm_types::flags().jit.deopt_null_const_as_object
}

pub(in crate::ir_lower) fn stack_arg_block_size(stack_arg_count: usize) -> (i32, i32) {
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

/// Most outgoing STACK words an inline-cache hit in this tier marshals (the
/// context ABI's count, the wider of the two). A bound on code size at one
/// site, not an ABI limit: every compiled prologue reads any number of caller
/// stack words. Since round 13 wave 11 (lane mega9, proposal M13-8) the one
/// cap every inline cache and the hashed stub share,
/// `inline_cache_pic::ic_max_stack_words` (8; 4 under
/// `CRATONVM_JIT_IC_WIDE_STACK_WORDS=0`), read per site compile.
pub(in crate::ir_lower) fn ir_ic_max_stack_words() -> usize {
    crate::inline_cache_pic::ic_max_stack_words()
}

/// Does a virtual / interface site with `num_args` Java arguments (receiver
/// included) get an inline cache in this tier? Round 11 wave 13 (lane
/// `icwide`): no longer "fits the register file with the context word". A
/// site that overflows it by up to [`ir_ic_max_stack_words`] context-ABI
/// words gets a cache whose hits marshal the overflow onto the stack
/// (`Lowerer::emit_ic_wide_hit_call`); `HashMap.newNode` (receiver + 4) on
/// Win64 is the site that paid a blind `jit_invoke_dispatch` per `put`.
///
/// The ONE predicate for both halves: `lib.rs`'s IC planning asks it before
/// allocating a MIC/PIC pair, and `Op::Call` asks it before lowering one, so
/// the planner cannot hand the lowerer a site it would drop.
///
/// Counted against `crate::ir_entry_abi_reg_count()`, the planner-side mirror
/// of `ENTRY_ABI_REGS` (`ic_declines_when_args_overflow_the_abi_register_file`
/// pins the two equal), so that mirror keeps its production reader.
#[inline]
pub(crate) fn ir_ic_admits_arity(num_args: usize) -> bool {
    num_args >= 1 && num_args + 1 <= crate::ir_entry_abi_reg_count() + ir_ic_max_stack_words()
}

/// `CRATONVM_JIT_IC_STACK_GUARD` (default on) for this tier's inline-cache
/// sites: `0` emits them without the stack sample, exactly as before round 13
/// wave 2. The same switch as the single-pass guard (`x64/op_invoke.rs`) and
/// the dispatch helpers' floor check; read per site compile.
pub(in crate::ir_lower) fn ir_ic_stack_guard_enabled() -> bool {
    true
}

/// Whether `id` is an `Op::CheckCast` (of `recv`, when given) whose target no
/// array is assignable to (JVMS 4.10.1.2: an array is assignable only to
/// `Object`, `Cloneable`, `Serializable` and array types), and which carries
/// no exception edge (a cast with a local handler has a path past it on which
/// it FAILED). An unnamed target answers `false`.
pub(in crate::ir_lower) fn cast_node_excludes_arrays(
    graph: &Graph,
    id: NodeId,
    recv: Option<NodeId>,
) -> bool {
    let Some(n) = graph.node_opt(id) else {
        return false;
    };
    let Op::CheckCast { name_ptr, name_len } = n.op else {
        return false;
    };
    if recv.is_some_and(|r| n.inputs.get(2) != Some(&r)) {
        return false;
    }
    // SAFETY: an `intern_typecheck_target` pair (leaked for the process's
    // life), or `(0, 0)`, which answers `None`.
    let Some(name) = (unsafe { crate::typecheck_site_name(name_ptr as *const u8, name_len) })
    else {
        return false;
    };
    if crate::inline_cache_pic::ic_static_receiver_admits_arrays(name) {
        return false;
    }
    !graph
        .nodes
        .iter()
        .any(|t| matches!(t.op, Op::CallThrew { .. }) && t.inputs.get(1) == Some(&id))
}

/// DISPATCH21-4 (`CRATONVM_JIT_IR_IC_CAST_KIND_SCREEN_ELIDE`): has a
/// `checkcast` of `recv` to a type no array is assignable to SUCCEEDED before
/// a call whose memory input is `mem`, on every path?
///
/// * `recv` itself such a cast: its value exists only on the cast's success.
/// * Otherwise (the default `CRATONVM_JIT_IR_CHECKCAST_VALUE_FORWARD` rewires
///   the call to read the cast's OPERAND), walk the memory chain back from
///   `mem`: a cast is `Opaque`, so every later memory operation is ordered
///   after it. The answer is "Start is unreachable backwards from the call
///   once the qualifying casts are removed", i.e. those casts dominate the
///   call; a memory phi fans out to every input (a loop's entry input always
///   reaches Start unless a cast precedes the loop), a revisited node adds
///   nothing. Because each cast USES `recv`, its definition dominates the
///   cast, so a cast on every path also lies after `recv`'s last definition
///   on every path: the object the call receives is the object the cast
///   checked.
/// * Anything the walk does not know (a node with no memory token, more than
///   `MAX_STEPS` nodes) answers `false`: the screen stays.
pub(crate) fn ic_receiver_cast_excludes_arrays(graph: &Graph, mem: NodeId, recv: NodeId) -> bool {
    const MAX_STEPS: usize = 256;
    if recv == NO_NODE {
        return false;
    }
    if cast_node_excludes_arrays(graph, recv, None) {
        return true;
    }
    if mem == NO_NODE {
        return false;
    }
    let mut work: Vec<NodeId> = vec![mem];
    let mut seen: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
    while let Some(cur) = work.pop() {
        if !seen.insert(cur) {
            continue;
        }
        if seen.len() > MAX_STEPS {
            return false;
        }
        let Some(n) = graph.node_opt(cur) else {
            return false;
        };
        if cast_node_excludes_arrays(graph, cur, Some(recv)) {
            continue;
        }
        match n.op {
            Op::Phi if n.ty == IrType::Memory => {
                if n.inputs.len() < 2 || n.inputs.iter().skip(1).any(|&m| m == NO_NODE) {
                    return false;
                }
                work.extend(n.inputs.iter().skip(1).copied());
            }
            Op::Proj(_) if n.ty == IrType::Memory => match n.inputs.get(0) {
                Some(&p) if p != NO_NODE => work.push(p),
                _ => return false,
            },
            _ => match crate::ir::memory_token_slot(n).and_then(|k| n.inputs.get(k)) {
                Some(&m) if m != NO_NODE => work.push(m),
                _ => return false,
            },
        }
    }
    true
}

// ── Lowering state ───────────────────────────────────────────────────

/// One `Op::Switch` site's jump table, recorded while the site is emitted and
/// laid out afterwards by [`Lowerer::emit_switch_tables`].
///
/// # Why the table is not emitted where it is used
///
/// A jump table is DATA. Emitted inline behind the indirect `JMP` it would sit
/// in the instruction stream, and the block laid out physically next would be
/// entered by decoding the table as instructions the moment anything reached
/// it by fall-through. The single-pass backend gets away with an inline table
/// only because its `tableswitch` arm marks the rest of the basic block dead;
/// this tier lays blocks out by frequency, so "nothing falls through to here"
/// is not a property any one site can establish. Deferring the bytes to the
/// tail — beside the deopt stub, the call-exception stub and the outlined
/// safepoint polls, which are there for the same reason — makes it a property
/// of the emission order instead.
pub(in crate::ir_lower) struct SwitchTablePlan {
    /// Native offset of the `disp32` in the site's `LEA RCX, [RIP + disp32]`,
    /// patched to the table's base once that base is known.
    pub(in crate::ir_lower) lea_patch: usize,
    /// The successor block of each table slot, in slot order: slot `i` is the
    /// edge for key `low + i`. A slot the payload did not list carries the
    /// default edge's block, so the table is total over `[low, high]`.
    pub(in crate::ir_lower) targets: Vec<usize>,
}

/// JIT round 19 wave 5 (lane handler5, SPLICE4-1): a call whose exceptional
/// exit may enter the compiled handler arm `ir::IrBuilder::note_catch_arm`
/// built, as [`Lowerer::catch_arm_wire`] proved it enterable.
#[derive(Clone, Debug)]
pub(in crate::ir_lower) struct CatchArmWire {
    /// Index into `Graph::catch_arm_sites`.
    pub(in crate::ir_lower) site: u16,
    /// The block the arm's `Proj(0)` starts.
    pub(in crate::ir_lower) arm_block: usize,
    /// Does that block read the caught throwable (`Op::CaughtException`
    /// survived DCE)? Without a reader the probe clears the parked word.
    pub(in crate::ir_lower) caught: bool,
    /// JIT round 19 wave 6 (lane handler6, HANDLER19-3): the ladder's further
    /// arms, `(site, arm block, caught)`, in chain order, each proved
    /// enterable like the first. Empty for a one-arm call (wave 5's shape).
    pub(in crate::ir_lower) more: Vec<(u16, usize, bool)>,
}

/// JIT round 19 wave 7 (lane handler7, HANDLER6-2 of
/// `jit-r19-handler6-proposals-RETIRED-20261009.md`): why an armed call's exceptional exit was
/// NOT wired to its compiled handler probe. Each is one clause of
/// [`Lowerer::catch_arm_wire`] (or of the two later refusals in
/// [`Lowerer::push_call_exc_patch`] / [`Lowerer::push_catch_probe`]), counted
/// in the local-handler census (`crate::metrics::LOCAL_HANDLER_NAMES`, the
/// `ir-wire-*` rows) and named by a `CRATONVM_DBG_JITC` line
/// (`ir-catch-wire refused <method> call pc=N: <clause>`). Reporting only:
/// every clause refuses exactly what it refused before.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum CatchWireRefusal {
    /// The call is not scheduled in the block its `If(CatchEntryFlag)` ends.
    CallNotPaired,
    /// A node is scheduled after the call in its block (a result user
    /// without `CRATONVM_JIT_IR_CATCH_ARM_SINK_RESULT_USERS`).
    TailAfterCall,
    /// The lookup helper, the context word or the scratch word is unwired.
    Unwired,
    /// Split residency, edge copies, MIR mode or reference residency.
    Residency,
    /// A self-locking body.
    SelfLock,
    /// The arm block has another predecessor, or something that can reach a
    /// safepoint is scheduled before the caught-throwable read.
    ArmLeg,
    /// The exit left with a shadow publication pending (an inline-cache
    /// `.tail` exit; HANDLER6-3).
    PendingShadow,
    /// No candidate row fit the probe's site.
    SiteBox,
}

impl CatchWireRefusal {
    pub(in crate::ir_lower) fn as_str(self) -> &'static str {
        match self {
            Self::CallNotPaired => "call-not-paired",
            Self::TailAfterCall => "tail-after-call",
            Self::Unwired => "unwired",
            Self::Residency => "residency",
            Self::SelfLock => "self-lock",
            Self::ArmLeg => "arm-leg",
            Self::PendingShadow => "pending-shadow",
            Self::SiteBox => "site-box",
        }
    }

    pub(in crate::ir_lower) fn metric(self) -> usize {
        use crate::metrics as m;
        match self {
            Self::CallNotPaired => m::LOCAL_HANDLER_IR_WIRE_CALL_NOT_PAIRED,
            Self::TailAfterCall => m::LOCAL_HANDLER_IR_WIRE_TAIL_AFTER_CALL,
            Self::Unwired => m::LOCAL_HANDLER_IR_WIRE_UNWIRED,
            Self::Residency => m::LOCAL_HANDLER_IR_WIRE_RESIDENCY,
            Self::SelfLock => m::LOCAL_HANDLER_IR_WIRE_SELF_LOCK,
            Self::ArmLeg => m::LOCAL_HANDLER_IR_WIRE_ARM_LEG,
            Self::PendingShadow => m::LOCAL_HANDLER_IR_WIRE_PENDING_SHADOW,
            Self::SiteBox => m::LOCAL_HANDLER_IR_WIRE_SITE_BOX,
        }
    }
}

/// HANDLER19-3: the most candidates one compiled handler probe dispatches
/// (`CMP EAX, imm8` per candidate; a longer prefix is cut here).
pub(in crate::ir_lower) const CATCH_LADDER_MAX: usize = 16;

/// SPLICE4-1: where a probe's miss goes -- the exit the call took before the
/// probe existed, recorded by the ordinary route and held back until the probe
/// is emitted ([`Lowerer::emit_catch_probes`]).
pub(in crate::ir_lower) enum CatchFallback {
    /// A reason-9 rethrow pad (a protected bci on the precise-frame route).
    Rethrow(RethrowPad),
    /// A `call_exc_patches` row, the shared exceptional exit, at this bci.
    Exit(usize),
}

/// SPLICE4-1: one exceptional exit routed through a local-handler probe.
pub(in crate::ir_lower) struct CatchProbe {
    /// The rel32 of the call's exceptional branch, patched to the probe.
    pub(in crate::ir_lower) patch: usize,
    pub(in crate::ir_lower) wire: CatchArmWire,
    /// Index into `Lowerer::catch_site_boxes`.
    pub(in crate::ir_lower) site_box: usize,
    pub(in crate::ir_lower) fallback: CatchFallback,
    /// JIT round 19 wave 7 (lane handler7, HANDLER6-3): the site's shadow
    /// publication when the exit left BEFORE its reload (the inline-cache
    /// `.tail` shape), admitted under `CRATONVM_JIT_IR_CATCH_ARM_SHADOW_EXITS`:
    /// the probe's hit edge reloads it (copy back and retract `top`) before it
    /// enters the arm. `None`: the reload already ran.
    pub(in crate::ir_lower) shadow: Option<Vec<i16>>,
}

/// One reason-9 rethrow pad, recorded by [`Lowerer::push_rethrow_pad`] at a
/// call-exception exit inside a protected range and emitted after the body by
/// [`Lowerer::emit_rethrow_pads`] (round 11 wave 10, lane irexc).
pub(in crate::ir_lower) struct RethrowPad {
    /// The rel32 of the site's exceptional branch, patched to the pad.
    pub(in crate::ir_lower) patch: usize,
    /// The boxed `PendingException` point the pad loads into `DEOPT_ARG0`.
    pub(in crate::ir_lower) point_ptr: u64,
    /// The throw-site bci the pad stamps (`set_throw_bci`), already
    /// `resume_bci`-mapped.
    pub(in crate::ir_lower) bci: usize,
    /// The site's shadow publication when the exceptional branch leaves
    /// BEFORE the site's reload ran (the inline-cache `.tail` shape):
    /// `(thread slot, save-base slot, published words)`. The pad copies the
    /// possibly-relocated references back into their homes before the deopt
    /// entry reads them. `None` when the reload already ran.
    pub(in crate::ir_lower) shadow: Option<(i32, i32, Vec<i16>)>,
}

/// One `Op::Call` armed with call-site exception edges (round nio21 wave 3,
/// lane L2; `ir::Op::CallThrew`), planned before emission by
/// [`Lowerer::plan_call_throw_edges`].
#[derive(Clone, Debug)]
pub(in crate::ir_lower) struct CallThrowPlan {
    /// The `JitLocalHandlerSite` the pad hands `jit_local_handler_lookup`,
    /// owned by `Lowerer::call_throw_sites` and then by the artifact's
    /// `_jit_local_handler_sites`.
    pub(in crate::ir_lower) site_ptr: u64,
    /// `targets[k]` = the block of the true edge of `If(CallThrew { k })`,
    /// the block the pad jumps into when the helper answers `k`.
    pub(in crate::ir_lower) targets: Vec<usize>,
    /// The armed `Op::Call`.
    pub(in crate::ir_lower) call: NodeId,
    /// nio21 wave 4 L2: the site is inside a spliced callee's `try`
    /// (`ir::IrCallThrowSite::spliced`): every exceptional exit of the call
    /// must reach the pad (a stack-guard exit excepted), or the compile is
    /// refused (`Lowerer::push_call_exc_patch`).
    pub(in crate::ir_lower) spliced: bool,
}

/// Where an armed call's exception goes when the helper answers `-1`: the
/// exit [`Lowerer::push_call_exc_patch`] would have recorded for the site,
/// decided while the call is the node being lowered and bound to the pad's
/// own `JMP` when the pad is emitted.
pub(in crate::ir_lower) enum CallThrowFallback {
    /// A plain `call_exc_patches` row at this bci.
    Plain { bci: usize },
    /// A reason-9 rethrow pad (protected bci), its point already built.
    Rethrow(RethrowPad),
}

/// A call past its publication boundary, with a token when one ID describes
/// its exceptional edge.
pub(in crate::ir_lower) struct CallThrowPublication {
    pub(in crate::ir_lower) map: Option<SafepointMapToken>,
}

/// One out-of-line call-site exception pad (round nio21 wave 3, lane L2),
/// recorded by [`Lowerer::push_call_throw_pad`] and emitted by
/// [`Lowerer::emit_call_throw_pads`].
pub(in crate::ir_lower) struct CallThrowPad {
    /// The map whose ID the incoming edge leaves in the frame word.
    pub(in crate::ir_lower) map: Option<SafepointMapToken>,
    /// The rel32 of the site's exceptional branch (entered with RAX ==
    /// `i64::MIN`), patched to the pad.
    pub(in crate::ir_lower) entry_patch: usize,
    pub(in crate::ir_lower) site_ptr: u64,
    pub(in crate::ir_lower) targets: Vec<usize>,
    /// The site's shadow publication when its reload had not run yet, as
    /// [`RethrowPad::shadow`].
    pub(in crate::ir_lower) shadow: Option<(i32, i32, Vec<i16>)>,
    /// A `Ref` call result's home word, nulled on the taken path: the pad
    /// skips the result store, and a later map in the handler names the word
    /// (every `Ref` home emitted before it), which must then hold null rather
    /// than an earlier execution's result no map has kept current.
    pub(in crate::ir_lower) result_home: Option<i32>,
    pub(in crate::ir_lower) fallback: CallThrowFallback,
}

/// Why a reason-9 frame could NOT be handed to the exceptional-frame
/// consumers as it stands, or `None` when it can (round 11 wave 10, lane
/// irexc; see [`Lowerer::push_rethrow_pad`]).
///
/// The bar is what every consumer can take without a fallback that
/// propagates: `route_jit_signal_exception` maps the locals with
/// `ir_deopt_locals` (an `Unsupported` / `MaterializationRequired` slot makes
/// it propagate), the OSR exception transfer refuses a virtual local or a
/// relock monitor (and propagates), and neither walks an inlined caller scope.
/// So: every local resumable, no virtual object anywhere, every monitor one
/// the compiled code really holds (`relock == false`) on a describable
/// reference, and no caller chain.
///
/// `osr_virtuals` (OSR round, 2026-10-06): the frame is an OSR-door body's,
/// whose pads only the OSR exception route reads, and that route re-allocates
/// scalar-replaced locals (`deopt::osr_exception_exit_virtuals_enabled`). A
/// virtual local is then admitted when that transfer can take it
/// ([`crate::deopt::osr_exception_exit_virtuals_refusal`]); every other rule
/// stands.
pub(in crate::ir_lower) fn rethrow_frame_refusal(
    fs: &FrameState,
    osr_virtuals: bool,
) -> Option<String> {
    if fs.caller.is_some() {
        return Some("an inlined caller scope".to_string());
    }
    let virtual_value = |v: &FrameValue| {
        matches!(
            v,
            FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_)
        )
    };
    let names_virtual_local = fs.locals.iter().any(virtual_value);
    if osr_virtuals && names_virtual_local {
        if !crate::deopt::frame_state_is_resumable(fs) {
            return Some("an unresumable local beside a scalar-replaced object".to_string());
        }
        if let Some(why) = crate::deopt::osr_exception_exit_virtuals_refusal(fs) {
            return Some(why.to_string());
        }
    } else {
        if let Some(slot) = crate::deopt::first_unresumable_local(fs) {
            return Some(slot);
        }
        if let Some(i) = fs.locals.iter().position(virtual_value) {
            return Some(format!("local {i} names a scalar-replaced object"));
        }
    }
    for m in &fs.monitors {
        if m.relock {
            return Some("a monitor the resume would have to re-acquire".to_string());
        }
        if virtual_value(&m.object)
            || matches!(
                m.object,
                FrameValue::Unsupported | FrameValue::MaterializationRequired(_)
            )
        {
            return Some(format!("an undescribable monitor object ({:?})", m.object));
        }
    }
    None
}

/// `self_lock_frame_refusal` for either kind of self-locking body. Round 15
/// wave 3 (lane sync2, S15-3): a `static synchronized` body's (`receiver ==
/// false`) monitor is its class mirror, and the VM's static arm of
/// `handed_method_monitor_index` has no `this` to compare, so local 0 is not
/// asked of it (a static method's local 0 is an argument, or nothing); the
/// rest of the shape is the instance one.
pub(in crate::ir_lower) fn self_lock_frame_refusal_for(
    fs: &FrameState,
    receiver: bool,
) -> Option<&'static str> {
    let is_ref =
        |v: &FrameValue| matches!(v, FrameValue::StackSlotRef(_) | FrameValue::RegisterRef(_));
    if fs.caller.is_some() {
        return Some("an inlined caller chain");
    }
    let [m] = fs.monitors.as_slice() else {
        return Some("the frame does not name the method monitor alone");
    };
    if m.relock || m.lock_depth != 1 {
        return Some("the method monitor is not one taken level");
    }
    if !is_ref(&m.object) {
        return Some("the monitor's object has no reference location");
    }
    if receiver && !fs.locals.first().is_some_and(is_ref) {
        return Some("local 0 does not name the receiver");
    }
    None
}

/// Round 15 wave 4 (lane sync3, proposal SY2-6 of `jit-r15w3-sync2-proposals-RETIRED-20261003.md`):
/// the rule of [`Lowerer::self_lock_refusal`] that refused a self-locking
/// body. Each is a census row (`ir-self-locking-refused-<name>=` on the
/// synchronized-splice census line, [`Self::census_row`]) and names the dbg
/// line, so the flip measurement of `CRATONVM_JIT_IR_SELF_LOCKING_SYNC` can say
/// which rule keeps which methods on the single-pass self-locking body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum SelfLockRefusal {
    /// The monitor is neither the receiver's published home nor a mirror slot.
    MonitorHome = 0,
    /// The compile has an exception-table range (item 5; proposal SY2-1).
    ProtectedRange = 1,
    /// The enter (or a static body's mirror read) is gone, unscheduled, or
    /// not first in an entry block nothing re-enters.
    Enter = 2,
    /// Something that can transfer is scheduled ahead of the enter.
    TransferBeforeEnter = 3,
    /// An exit is not one `MonitorExit` of the monitor in a returning block.
    ExitShape = 4,
    /// Something that can transfer is scheduled after an exit.
    TransferAfterExit = 5,
    /// A live `Op::Return` whose block releases nothing.
    ReturnNotReleased = 6,
    /// A deopt exit, with a hand-over switch off.
    HandoverOff = 7,
    /// A deopt frame with an inlined caller chain.
    FrameChain = 8,
    /// A flat deopt frame that does not name the method monitor alone, as one
    /// taken level located as a reference (and, for an instance body, local 0).
    FrameShape = 9,
}

impl SelfLockRefusal {
    /// Every cause, in census order.
    #[cfg(test)]
    pub(in crate::ir_lower) const ALL: [Self; 10] = [
        Self::MonitorHome,
        Self::ProtectedRange,
        Self::Enter,
        Self::TransferBeforeEnter,
        Self::ExitShape,
        Self::TransferAfterExit,
        Self::ReturnNotReleased,
        Self::HandoverOff,
        Self::FrameChain,
        Self::FrameShape,
    ];

    /// The row's suffix and the dbg line's tag.
    pub(in crate::ir_lower) fn name(self) -> &'static str {
        match self {
            Self::MonitorHome => "monitor-home",
            Self::ProtectedRange => "protected-range",
            Self::Enter => "enter",
            Self::TransferBeforeEnter => "transfer-before-enter",
            Self::ExitShape => "exit-shape",
            Self::TransferAfterExit => "transfer-after-exit",
            Self::ReturnNotReleased => "return-not-released",
            Self::HandoverOff => "handover-off",
            Self::FrameChain => "frame-chain",
            Self::FrameShape => "frame-shape",
        }
    }

    /// Its row of the synchronized-splice census.
    pub(in crate::ir_lower) fn census_row(self) -> usize {
        // Cast: a fieldless enum's discriminant, 0..=9.
        crate::ir::SYNC_CENSUS_IR_SELF_LOCK_REFUSED_CAUSE_FIRST + self as usize
    }
}

/// `CRATONVM_JIT_IR_FRESH_VOLATILE_STORE_UNFENCED` (JIT round 21 wave 22, lane
/// cf, `r21-completablefuture-stage-cost-is-allocation-and-calls-20261018-RETIRED-20261024.md`;
/// perf, **default ON** since R24-KPC-FLIP, `=0` keeps every fence; flip
/// review #2 SAFE-TO-FLIP): the optimizing tier skips the StoreLoad fence after
/// a volatile `putfield` into an object this compile allocated, when no use
/// that could publish the object can run between the allocation and the store
/// ([`fresh_allocation_unfenced_volatile_stores`]). Read once per compile, at
/// its first volatile store.
pub(crate) fn ir_fresh_volatile_store_unfenced_enabled() -> bool {
    // R24-KPC-FLIP: on the CF `done` row the trailing `LOCK ADD` after
    // `uniApplyNow`'s `d.result = ..` store was ~50 % of the samples.
    true
}

/// One scheduled block as [`fresh_allocation_unfenced_volatile_stores`] reads
/// it: `Block::nodes`, `Block::terminator`, `Block::successors`.
#[derive(Clone, Copy)]
pub(in crate::ir_lower) struct FenceBlockView<'s> {
    pub(in crate::ir_lower) nodes: &'s [NodeId],
    pub(in crate::ir_lower) terminator: Option<NodeId>,
    pub(in crate::ir_lower) successors: &'s [usize],
}

/// What the entry fast return's outlined stub puts in RAX (round 12 wave 8,
/// lane callcost; [`Lowerer::emit_entry_fast_return`]): the incoming ABI
/// register of an `int` parameter, copied whole exactly as the full path's
/// `Op::Return` copies the prologue word that register was stored to, or an
/// `int` constant's 64-bit value exactly as `Op::Const` materialises it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum EntryFastValue {
    Reg(u8),
    Imm(i64),
}

/// The entry fast return's plan: `CMP <cmp_reg>32, imm; J<ret_cc> stub`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) struct EntryFastReturn {
    pub(in crate::ir_lower) cmp_reg: u8,
    pub(in crate::ir_lower) imm: i32,
    /// The condition under which the RETURN edge is taken, for `cmp_reg - imm`.
    pub(in crate::ir_lower) ret_cc: CmpOp,
    pub(in crate::ir_lower) value: EntryFastValue,
    /// The `Op::Param` node `cmp_reg` carries (round 14 wave 5, lane callcost:
    /// the entry fold evaluates the body for values of it).
    pub(in crate::ir_lower) param: NodeId,
}

/// Round 14 wave 5 (lane callcost, proposal CC5-5; [`Lowerer::emit_entry_fold`]):
/// the arguments just past the entry fast return for which the body is
/// call-free and pure, folded to what it returns. `values[i]` is the body's
/// `int` result for argument `lo + i`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) struct EntryFold {
    pub(in crate::ir_lower) cmp_reg: u8,
    pub(in crate::ir_lower) lo: i32,
    pub(in crate::ir_lower) values: Vec<i32>,
    /// Why the range ends where it does (the census line).
    pub(in crate::ir_lower) stop: String,
    /// How many distinct self-call arguments the evaluation answered (round
    /// 14 wave 7; the census line).
    pub(in crate::ir_lower) self_calls: usize,
}

/// Round 14 wave 6 (lane callcost2, proposal CE-1 of
/// `jit-r14-callcost-proposals-RETIRED-20260930.md`; [`Lowerer::emit_self_call_answer`]): what
/// this body's own entry answers without a frame, recorded when the entry
/// fast return (and the entry fold, if any) is emitted, so a direct self-call
/// whose argument falls in that range is answered in the CALLER. `param` is
/// the compared `Op::Param`; the fast return is taken on `p <ret_cc> imm` and
/// returns `value`; `fold` is `(lo, values)`, the entry fold's table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) struct SelfCallAnswer {
    pub(in crate::ir_lower) param: NodeId,
    pub(in crate::ir_lower) imm: i32,
    pub(in crate::ir_lower) ret_cc: CmpOp,
    pub(in crate::ir_lower) value: EntryFastValue,
    pub(in crate::ir_lower) fold: Option<(i32, Vec<i32>)>,
}

/// Round 14 wave 6 (lane callcost2, CE-1): `CRATONVM_JIT_IR_SELF_CALL_ANSWER`,
/// default ON. `0` answers no direct self-call in the caller (every one makes
/// its `CALL`, the round-14-wave-5 shape). Read per compile, where the entry
/// fast return is planned.
pub(in crate::ir_lower) fn ir_self_call_answer_enabled() -> bool {
    true
}

/// Round 16 (lane codegen, CG16-1): `CRATONVM_JIT_IR_ENTRY_FAST_RETURN_PHI`,
/// default ON. `0` keeps the entry fast return to a return block entered from
/// block 0 alone, so `return n < 2 ? n : f(n - 1) + f(n - 2);` (javac's
/// ternary: both arms meet at one `ireturn` of a phi) gets no fast return, no
/// entry fold and no caller-side answer -- the pre-round-16 shape, measured on
/// the round-16 base binary at 17.5 ns per `fib` call against 2.9 for the same
/// function written `if (n <= 1) return n;` (`R16CodegenLoops` fibT / fibI).
/// Read per compile, where the entry fast return is planned.
pub(in crate::ir_lower) fn ir_entry_fast_return_phi_enabled() -> bool {
    true
}

/// Round 16 wave 3 (lane codegen, CG16-6): `CRATONVM_JIT_IR_PHI_COALESCE_BACK_EDGE=1`
/// gives a loop phi's back-edge `Add`/`Sub` source the phi's own register
/// (`Lowerer::coalesce_back_edge_phi_sources`), so the back edge's copy, and
/// for a source the residency rules left in memory its home reload, go away.
/// default on (not measured). It engages where the source is computed after
/// the phi's last read in the latch, which the default placement rarely does
/// (the source sits in the header, above the exit test); pair it with
/// `CRATONVM_JIT_IR_SINK_EQUAL_DEPTH=1`. Read per compile.
///
/// JIT round 23 wave 3b (lane L1, L1W3-1): with the switch unset, ON for a
/// graph the production pipeline marked [`Graph::loop_carried_regs`]
/// (`CRATONVM_JIT_IR_LOOP_CARRIED_REGS`, which also turns on the integer
/// equal-depth sink this pairs with). An explicit value wins either way.
/// `CRATONVM_JIT_IR_PHI_COALESCE_FOREIGN_PHI` (JIT round 23 wave 5b, lane L1;
/// perf, **default ON**, `=0` kill): back-edge phi coalescing
/// ([`ir_phi_coalesce_back_edge_enabled`]) no longer refuses a source because a
/// phi of ANOTHER merge holds the same register, when no edge copy of that phi
/// can land in the source's block (`Lowerer::foreign_phi_spares_block`). Read
/// per coalescing candidate.
pub(crate) fn ir_phi_coalesce_foreign_phi_enabled() -> bool {
    true
}

pub(in crate::ir_lower) fn ir_phi_coalesce_back_edge_enabled(graph: &Graph) -> bool {
    match crate::ir_schedule::explicit_switch("CRATONVM_JIT_IR_PHI_COALESCE_BACK_EDGE") {
        Some(explicit) => explicit,
        None => graph.loop_carried_regs,
    }
}

/// `CRATONVM_JIT_IR_LOOP_CARRIED_REGS` (JIT round 23 wave 3b, lane L1, L1W3-1;
/// perf, **default ON**, `=0` kill): keep a loop's carried integer values in
/// registers across the back edge and its poll. The production pipeline
/// (`lib.rs`) marks the graph ([`Graph::loop_carried_regs`]) before
/// scheduling; the scheduler then sinks an integer back-edge source into the
/// latch at equal loop depth (`ir_schedule::sink_pure_nodes`), and this
/// lowerer gives it its phi's register
/// (`Lowerer::coalesce_back_edge_phi_sources`). Measured on `n23-w2.exe`
/// (`PF.plain`, `s += o.v + h.k + i`): the latch's
/// `lea rax,..; mov [rbp-0D0h],rax; ..; poll; mov r13,[rbp-0D0h]` (a store and a
/// reload per carried value per iteration) became `lea r13,[r13+rcx]; lea
/// ebx,[rbx+1]; poll`. Both halves are the round-16 mechanisms (CG16-6 and the
/// equal-depth rule) with their own soundness checks: the sink's
/// `placement_is_sound` revert, and the coalescing's exclusive-holder and
/// pure-tail rules, which keep `deopt_nameable` exact so a deopt or a poll exit
/// at the back edge names the register only where it holds the value. Integer
/// phis only; a reference is never coalesced. Read once per compile.
pub(crate) fn ir_loop_carried_regs_enabled() -> bool {
    true
}

/// Round 16 wave 3 (lane codegen, CG16-7; backlog CG-B1):
/// `CRATONVM_JIT_IR_PHI_CONST_SOURCE_IMM=1` reads a constant phi source as an
/// immediate (`MOV r, imm`) in `emit_copy_op` instead of loading the
/// constant's home word, as every other reader of a constant already does.
/// Default OFF (not measured: one load per loop entry). Read per copy.
pub(in crate::ir_lower) fn ir_phi_const_source_imm_enabled() -> bool {
    false
}

/// Round 16 wave 4 (lane codegen, CG16-8;
/// `r16-codegen-hashmap-get-anatomy-FIXED-20261005.md`): `CRATONVM_JIT_IR_PROLOGUE_ZERO_RAX=1`
/// encodes the method prologue's frame-word zeroing as `XOR EAX, EAX` once per
/// run and `MOV [rbp - off], RAX` per word (4 or 7 bytes) instead of
/// `MOV qword [rbp - off], 0` (11 bytes). Same stores, same words, same values:
/// an encoding change. `HashMap.getNode`'s optimizing body zeroes 22 words per
/// call, 242 bytes of its prologue. Default OFF (not measured). Read per
/// compile.
pub(in crate::ir_lower) fn ir_prologue_zero_rax_enabled() -> bool {
    false
}

/// Round 16 (lane codegen, CG16-2; proposal 11 of
/// `jit-r11-irlower-proposals-RETIRED-20260928.md`, "rarely-taken paths out of
/// line"): `CRATONVM_JIT_IR_DIV_COLD_FIXUPS`, default ON since round 16
/// (measured: `R16CodegenLoops` nocall 2.51→1.51, intLoop 1.06→0.73, bigA
/// 64→55; `0`/`false`/`off`/`no` restores the inline shape). On, it moves an
/// integer `IDIV`'s two rare fixups -- the placeholder of a guarded zero divisor and
/// the answer for a divisor of `-1` -- after the body, so the hot path is
/// `TEST; JZ (not taken); CMP; JE (not taken); CDQ; IDIV` instead of two TAKEN
/// branches over inline fixups (`R16CodegenLoops` nocall: a `%` by an array
/// length per iteration). Off under a MIR mode, whose byte oracle compares
/// the inline shape. Read per division.
pub(in crate::ir_lower) fn ir_div_cold_fixups_enabled() -> bool {
    true
}

/// Where a division's fixup continues (round 16, lane codegen, CG16-2): the
/// inline shape's forward `JMP rel32` operand, patched to the post-`IDIV`
/// continuation by [`Lowerer::patch_div_overflow_after`], or the index of a
/// [`DivColdFixup`] whose resume that call records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum DivFixupPatch {
    Inline(usize),
    Cold(usize),
}

/// One frame load [`Lowerer::apply_home_reload_forwards`] may rewrite: the
/// store it follows starts at `store_start`, the load spans `start..end`
/// (`store_start..end` held `bytes` when it was emitted), and `dst` is the
/// register it loads (`src` the one the store wrote).
#[derive(Clone, Debug)]
pub(in crate::ir_lower) struct HomeReloadForward {
    pub(in crate::ir_lower) store_start: usize,
    pub(in crate::ir_lower) start: usize,
    pub(in crate::ir_lower) end: usize,
    pub(in crate::ir_lower) src: u8,
    pub(in crate::ir_lower) dst: u8,
    pub(in crate::ir_lower) bytes: Vec<u8>,
}

/// The recommended multi-byte `NOP`s (`0F 1F /0`), by length 1..=8.
pub(in crate::ir_lower) const HOME_RELOAD_NOPS: [&[u8]; 9] = [
    &[],
    &[0x90],
    &[0x66, 0x90],
    &[0x0F, 0x1F, 0x00],
    &[0x0F, 0x1F, 0x40, 0x00],
    &[0x0F, 0x1F, 0x44, 0x00, 0x00],
    &[0x66, 0x0F, 0x1F, 0x44, 0x00, 0x00],
    &[0x0F, 0x1F, 0x80, 0x00, 0x00, 0x00, 0x00],
    &[0x0F, 0x1F, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00],
];

/// The positions in `code` some instruction may transfer control to, among
/// `wanted`: any rel32 field (whatever its opcode) whose target is one, a
/// rel8 `JMP` / `Jcc` / `LOOP` / `JRCXZ` operand whose target is one, and any
/// 8-byte word equal to `base` plus one (an absolute address). Conservative
/// by construction: a byte run that only looks like a branch costs one kept
/// load, never a wrong one.
pub(in crate::ir_lower) fn home_reload_targets(
    code: &[u8],
    base: u64,
    wanted: &std::collections::HashSet<usize>,
) -> std::collections::HashSet<usize> {
    home_reload_targets_except(code, base, wanted, &std::collections::HashSet::new())
}

/// [`home_reload_targets`], ignoring the rel32 FIELDS at `exempt` (offsets of
/// the field, not of the instruction): branches whose target the caller has
/// proved harmless. JIT round 25 item 1a: the post-call exit stubs' `JZ` back
/// to the site's end, whose RAX the verdict tail preserves.
pub(in crate::ir_lower) const MAX_OFFSET_BITSET_INDEX: usize = 1 << 20;

#[inline]
pub(in crate::ir_lower) fn build_offset_bitset(
    set: &std::collections::HashSet<usize>,
    max_incl: usize,
) -> Vec<u64> {
    if set.is_empty() {
        return Vec::new();
    }
    let cap = max_incl.min(MAX_OFFSET_BITSET_INDEX);
    let mut bits = vec![0u64; (cap >> 6) + 1];
    for &idx in set {
        if idx <= cap {
            bits[idx >> 6] |= 1u64 << (idx & 63);
        }
    }
    bits
}

#[inline]
pub(in crate::ir_lower) fn bitset_or_set_contains(
    bits: &[u64],
    set: &std::collections::HashSet<usize>,
    idx: usize,
) -> bool {
    if let Some(&w) = bits.get(idx >> 6) {
        (w & (1u64 << (idx & 63))) != 0
    } else {
        set.contains(&idx)
    }
}

pub(in crate::ir_lower) fn home_reload_targets_except(
    code: &[u8],
    base: u64,
    wanted: &std::collections::HashSet<usize>,
    exempt: &std::collections::HashSet<usize>,
) -> std::collections::HashSet<usize> {
    let mut hit = std::collections::HashSet::new();
    let Some(&last) = wanted.iter().max() else {
        return hit;
    };
    let len = code.len();
    let wanted_bits = build_offset_bitset(wanted, last);
    let exempt_bits = build_offset_bitset(exempt, len);
    let has_exempt = !exempt_bits.is_empty();
    for i in 0..len {
        if i + 4 <= len {
            let rel = i32::from_le_bytes([code[i], code[i + 1], code[i + 2], code[i + 3]]);
            let t = (i as i64 + 4 + i64::from(rel)) as u64;
            if t <= last as u64
                && bitset_or_set_contains(&wanted_bits, wanted, t as usize)
                && (!has_exempt || !bitset_or_set_contains(&exempt_bits, exempt, i))
            {
                hit.insert(t as usize);
            }
        }
        if i >= 1 && matches!(code[i - 1], 0x70..=0x7F | 0xE0..=0xE3 | 0xEB) {
            let t = (i as i64 + 1 + i64::from(code[i] as i8)) as u64;
            if t <= last as u64 && bitset_or_set_contains(&wanted_bits, wanted, t as usize) {
                hit.insert(t as usize);
            }
        }
        if i + 8 <= len {
            let mut w = [0u8; 8];
            w.copy_from_slice(&code[i..i + 8]);
            let v = u64::from_le_bytes(w);
            if let Some(off) = v.checked_sub(base) {
                if let Ok(off) = usize::try_from(off) {
                    if off <= last && bitset_or_set_contains(&wanted_bits, wanted, off) {
                        hit.insert(off);
                    }
                }
            }
        }
    }
    hit
}

/// JIT r25 W7 fix 4: [`home_reload_targets_except`] with its rel32 half
/// narrowed to fields a control transfer can own: a field right behind a
/// `CALL rel32` (`E8`), a `JMP rel32` (`E9`) or a `Jcc rel32` (`0F 80..8F`)
/// byte, and the `disp32` of a RIP-relative `LEA` (`8D`, ModRM `00 reg 101`),
/// which is how a resume address is taken. The rel8 and absolute-word halves
/// are the same. Still byte-pattern conservative -- the opcode bytes are not
/// checked to start an instruction, so a run that merely looks like one
/// counts -- but a frame displacement (`[rbp - 0x108]` read as a rel32 aimed a
/// few hundred bytes back) or an immediate no longer does, which was most of
/// the routed-result refusals the W7 census named (83 retried lowerings over
/// six regex rows). Every in-code transfer this backend emits is one of
/// these forms: the jump tables hold base-relative entries that name block
/// starts, never a position inside a node. Pure.
pub(in crate::ir_lower) fn branch_targets_except(
    code: &[u8],
    base: u64,
    wanted: &std::collections::HashSet<usize>,
    exempt: &std::collections::HashSet<usize>,
) -> std::collections::HashSet<usize> {
    let mut hit = std::collections::HashSet::new();
    let Some(&last) = wanted.iter().max() else {
        return hit;
    };
    let wanted_bits = build_offset_bitset(wanted, last);
    for_each_branch_field(code, base, exempt, |_, t, _| {
        if t <= last && bitset_or_set_contains(&wanted_bits, wanted, t) {
            hit.insert(t);
        }
    });
    hit
}

/// W7 fix 4: every control-transfer field [`branch_targets_except`] counts,
/// as `(field offset, target, kind)` -- `rel32`, `rel8` or `abs64`. The one
/// walk both the scan and the refusal context of
/// `Lowerer::verify_reg_alloc_routed_defs` read, so the message names exactly
/// the field the scan counted. Pure.
pub(in crate::ir_lower) fn for_each_branch_field(
    code: &[u8],
    base: u64,
    exempt: &std::collections::HashSet<usize>,
    mut f: impl FnMut(usize, usize, &'static str),
) {
    let len = code.len();
    let exempt_bits = build_offset_bitset(exempt, len);
    let has_exempt = !exempt_bits.is_empty();
    for i in 0..len {
        let rel32_owner = (i >= 1 && matches!(code[i - 1], 0xE8 | 0xE9))
            || (i >= 2 && code[i - 2] == 0x0F && matches!(code[i - 1], 0x80..=0x8F))
            || (i >= 2 && code[i - 2] == 0x8D && code[i - 1] & 0xC7 == 0x05);
        if rel32_owner
            && i + 4 <= len
            && (!has_exempt || !bitset_or_set_contains(&exempt_bits, exempt, i))
        {
            let rel = i32::from_le_bytes([code[i], code[i + 1], code[i + 2], code[i + 3]]);
            if let Ok(t) = usize::try_from(i as i64 + 4 + i64::from(rel)) {
                f(i, t, "rel32");
            }
        }
        if i >= 1 && matches!(code[i - 1], 0x70..=0x7F | 0xE0..=0xE3 | 0xEB) {
            if let Ok(t) = usize::try_from(i as i64 + 1 + i64::from(code[i] as i8)) {
                f(i, t, "rel8");
            }
        }
        if i + 8 <= len {
            let mut w = [0u8; 8];
            w.copy_from_slice(&code[i..i + 8]);
            let v = u64::from_le_bytes(w);
            if let Some(off) = v.checked_sub(base) {
                if let Ok(off) = usize::try_from(off) {
                    f(i, off, "abs64");
                }
            }
        }
    }
}

/// One out-of-line division fixup (round 16, lane codegen, CG16-2): the site's
/// `Jcc rel32` operand, the fixup's bytes (`XOR EAX, EAX`, `NEG EAX` or `NEG
/// RAX`; at most three), and the post-`IDIV` offset its `JMP` returns to (set
/// once the `IDIV` is emitted).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) struct DivColdFixup {
    pub(in crate::ir_lower) jcc_patch: usize,
    pub(in crate::ir_lower) fix: [u8; 3],
    pub(in crate::ir_lower) fix_len: u8,
    pub(in crate::ir_lower) resume: Option<usize>,
}

/// At most this many arguments are folded past the entry fast return: the
/// outlined stub is one `CMOVE` per value, so the cap bounds its length.
pub(in crate::ir_lower) const ENTRY_FOLD_MAX_VALUES: usize = 4;

/// At most this many blocks are walked by one evaluation of the body.
pub(in crate::ir_lower) const ENTRY_FOLD_MAX_STEPS: usize = 64;

/// Round 14 wave 7 (lane callcost3): at most this many nested direct
/// self-calls are evaluated below one folded argument, and at most
/// [`ENTRY_FOLD_MAX_ACTIVATIONS`] distinct callee arguments per compile
/// ([`Lowerer::entry_fold_self_call`]).
pub(in crate::ir_lower) const ENTRY_FOLD_MAX_DEPTH: usize = 8;
pub(in crate::ir_lower) const ENTRY_FOLD_MAX_ACTIVATIONS: usize = 64;

/// Round 14 wave 7 (lane callcost3): `CRATONVM_JIT_IR_ENTRY_FOLD_SELF_CALLS`,
/// default ON. `0` makes the entry fold stop at the first direct self-call on
/// the evaluated path (the round-14-wave-6 fold). Read per compile.
pub(in crate::ir_lower) fn ir_entry_fold_self_calls_enabled() -> bool {
    true
}

/// Round 14 wave 7 (lane callcost3, proposal CC6-2):
/// `CRATONVM_JIT_IR_PARAM_REGISTER_FILL`, default ON. `0` leaves every
/// parameter's register to its `Op::Param` node's reload from the prologue
/// word (the round-14-wave-6 entry). Read per compile.
pub(in crate::ir_lower) fn ir_param_register_fill_enabled() -> bool {
    true
}

/// The memo and budgets of one compile's entry-fold evaluation through direct
/// self-calls ([`Lowerer::entry_fold_self_call`]).
pub(in crate::ir_lower) struct EntryFoldCalls {
    pub(in crate::ir_lower) enabled: bool,
    /// Callee argument -> the body's result for it.
    pub(in crate::ir_lower) memo: HashMap<i32, i32>,
    /// The arguments whose evaluation is in progress, outermost first.
    pub(in crate::ir_lower) active: Vec<i32>,
    pub(in crate::ir_lower) activations: usize,
}

/// `CRATONVM_JIT_IR_FP_MINMAX_AVX`, **default on** (default on) -- JIT round 19
/// wave 4, lane irops3, IROPS19-5 of `docs/feature-designs/jit-ir-optimizations.md`: an IR FP
/// `Math.min`/`max` whose two operands are both register-resident lowers to
/// the branch-free [`fp_minmax_avx_bytes`] instead of the `UCOMIS; JP; JNE`
/// three-arm form, where the host has AVX (`VBLENDVPD` and the three-operand
/// forms are VEX-only). Read per site; no `static`
/// (`process_global_statics_ratchet`).
pub(in crate::ir_lower) fn ir_fp_minmax_avx_enabled() -> bool {
    crate::x64::has_avx()
}

/// One register-direct three-byte-VEX instruction (`VEX.128`, `L = 0`):
/// `map` 1 = `0F`, 3 = `0F3A`; `pp` 0 = none, 1 = `66`, 2 = `F3`, 3 = `F2`;
/// `reg` the ModRM.reg (destination), `vvvv` the first source, `rm` the
/// ModRM.rm source. Any of the sixteen XMM registers in every position
/// (`R̄`/`B̄` and the inverted `vvvv` carry bit 3).
pub(in crate::ir_lower) fn vex3_rr(
    out: &mut Vec<u8>,
    map: u8,
    pp: u8,
    reg: u8,
    vvvv: u8,
    rm: u8,
    opcode: u8,
) {
    let r_bar = if reg < 8 { 0x80 } else { 0 };
    let b_bar = if rm < 8 { 0x20 } else { 0 };
    // X̄ = 1 (no index register), W = 0 (WIG / W0 for every form used here).
    out.extend_from_slice(&[
        0xC4,
        r_bar | 0x40 | b_bar | map,
        ((!vvvv & 0x0F) << 3) | pp,
        opcode,
        0xC0 | ((reg & 7) << 3) | (rm & 7),
    ]);
}

/// JIT round 19 wave 4 (lane irops3, IROPS19-5): Java's FP `Math.min` /
/// `Math.max` of `a` (register `a`) and `b` (register `b`), branch-free, into
/// XMM0, writing only XMM0 and XMM1 (the value tier's scratch; `a` and `b`
/// are read in place and never written, so they may be the register cache's
/// XMM2+ or the same register).
///
/// The JDK bodies return the NaN ARGUMENT (`a` first, exact bits), order
/// `-0.0` below `+0.0`, and otherwise the smaller / larger value. `MINSD x, y`
/// is `x < y ? x : y` -- its SECOND operand on equality and on any NaN -- so:
///
/// ```text
///   VMINSD    xmm0, b, a        ; b < a ? b : a   (== -> a)
///   VMINSD    xmm1, a, b        ; a < b ? a : b   (== -> b)
///   VORPD     xmm0, xmm0, xmm1  ; ordered: the minimum; equal: a | b, which is
///                               ;   -0.0 if either zero is, else a == b itself
///   VCMPSD    xmm1, b, b, UNORD ; b is NaN?
///   VBLENDVPD xmm0, xmm0, b, xmm1   ; then b
///   VCMPSD    xmm1, a, a, UNORD ; a is NaN?
///   VBLENDVPD xmm0, xmm0, a, xmm1   ; then a (last: a NaN wins over b NaN)
/// ```
///
/// `max` is the same with `VMAXSD` and `VANDPD` (equal zeros: `+0.0` unless
/// both are `-0.0`). A NaN is selected, never computed, so its payload and a
/// signalling NaN's quiet bit come back exactly as passed -- the same answers,
/// bit for bit, as the three-arm form above and the JDK body (HotSpot's own
/// AVX sequence, which blends on the sign of `a` first, answers `b` for two
/// NaNs when `a`'s sign bit is set; this one does not). Float uses the `SS` /
/// `PS` forms (`VBLENDVPS` tests each dword's sign, and the compare's mask is
/// the low dword).
pub(in crate::ir_lower) fn fp_minmax_avx_bytes(dbl: bool, is_min: bool, a: u8, b: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(40);
    let scalar_pp = if dbl { 3 } else { 2 }; // F2 (SD) / F3 (SS)
    let packed_pp = if dbl { 1 } else { 0 }; // 66 (PD) / none (PS)
    let minmax = if is_min { 0x5D } else { 0x5F };
    let bitop = if is_min { 0x56 } else { 0x54 }; // VORP / VANDP
    let blendv = if dbl { 0x4B } else { 0x4A }; // VBLENDVPD / VBLENDVPS
    vex3_rr(&mut out, 1, scalar_pp, XMM0, b, a, minmax);
    vex3_rr(&mut out, 1, scalar_pp, XMM1, a, b, minmax);
    vex3_rr(&mut out, 1, packed_pp, XMM0, XMM0, XMM1, bitop);
    for x in [b, a] {
        vex3_rr(&mut out, 1, scalar_pp, XMM1, x, x, 0xC2);
        out.push(0x03); // UNORD_Q
        vex3_rr(&mut out, 3, 1, XMM0, XMM0, x, blendv);
        out.push(XMM1 << 4); // is4: the mask register
    }
    out
}

/// Java `int` semantics of the ops the entry fold evaluates: wrapping
/// arithmetic, shift distances masked to five bits, and `Cmp` as the `0`/`1`
/// the lowering materialises for it. `None` for any other op or arity.
pub(in crate::ir_lower) fn entry_fold_int_op(op: &Op, a: i32, b: Option<i32>) -> Option<i32> {
    Some(match (op, b) {
        (Op::Add, Some(b)) => a.wrapping_add(b),
        (Op::Sub, Some(b)) => a.wrapping_sub(b),
        (Op::Mul, Some(b)) => a.wrapping_mul(b),
        (Op::And, Some(b)) => a & b,
        (Op::Or, Some(b)) => a | b,
        (Op::Xor, Some(b)) => a ^ b,
        // Cast: `b & 31` is in `0..=31`.
        (Op::Shl, Some(b)) => a.wrapping_shl((b & 31) as u32),
        (Op::Shr, Some(b)) => a >> (b & 31),
        // Cast: the unsigned reading of the same 32 bits, and back.
        (Op::UShr, Some(b)) => ((a as u32) >> (b & 31)) as i32,
        (Op::Neg, None) => a.wrapping_neg(),
        (Op::Cmp(cc), Some(b)) => i32::from(match cc {
            CmpOp::Eq => a == b,
            CmpOp::Ne => a != b,
            CmpOp::Lt => a < b,
            CmpOp::Le => a <= b,
            CmpOp::Gt => a > b,
            CmpOp::Ge => a >= b,
        }),
        _ => return None,
    })
}

/// Where a deferred self-call sentinel path stages one argument for the
/// callee-deopt service (round 13 wave 8, lane callcost3): exactly the source
/// `gp_load_value(RAX, arg)` would have read AT the call, captured there
/// because the tail is emitted after the body, where the lowering state no
/// longer describes the call. Every source still holds the value when the
/// tail runs: the tail is entered straight from the `JO` after the `CALL`, a
/// resident register is one of the callee-saved GP file, and a home word is
/// not written by the callee.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum SelfCallArgSource {
    Imm(i64),
    Reg(u8),
    Home(i32),
}

/// zs s9/ca4: one split inline ZGC allocation's cold half
/// ([`Lowerer::emit_deferred_zgc_alloc_tails`]): the helper arm and the stub,
/// then the identity republish, the null test back to `resume` and the
/// exception exit -- everything the unsplit site emitted after its fast path.
pub(in crate::ir_lower) struct DeferredZgcAllocTail {
    pub(in crate::ir_lower) map: Option<SafepointMapToken>,
    pub(in crate::ir_lower) kind: DeferredZgcAllocKind,
    pub(in crate::ir_lower) split: crate::runtime_lowering::ZgcAllocSplit,
    /// The site's continuation, where the inline arm fell through.
    pub(in crate::ir_lower) resume: usize,
    /// The exit row's bci ([`Lowerer::note_deferred_cross_call_exit`]).
    pub(in crate::ir_lower) exit_bci: usize,
    /// `cur_node_pc` / `cur_bci` at the site, for the stub's return-site rows.
    pub(in crate::ir_lower) node_pc: Option<usize>,
    pub(in crate::ir_lower) site_bci: usize,
}

/// JIT round 23 wave 9 (lane W9-L7, `CRATONVM_JIT_IR_GETFIELD_COLD_TAIL`):
/// one inline field read's cold half, recorded at the site and emitted after
/// the body by [`Lowerer::emit_deferred_getfield_tails`]. A compact-layout
/// site puts its legacy read in the tail; a legacy-only site emits that read
/// inline. Both defer the checked `jit_getfield` helper, entered from the
/// site's failed guards, and return to the same result join.
///
/// Everything the tail emits is a per-compile constant or was captured at
/// the site: the base's location, the helper's `field_index` argument word
/// (with its NPE-site key), the bci the exception exit names. Nothing between
/// the site's branch and the tail writes a register or a frame word.
pub(in crate::ir_lower) enum DeferredPutfieldTail {
    Helper {
        jumps: Vec<usize>,
        base_slot: i32,
        field_index: i64,
        value_slot: i32,
        done: usize,
    },
    G1 {
        cross_region: Option<usize>,
        to_helper: Vec<usize>,
        cell_off: i32,
        base_slot: i32,
        field_index: i64,
        value_slot: i32,
        done: usize,
    },
}

pub(in crate::ir_lower) enum DeferredGenAllocKind {
    New {
        class_id: u32,
        num_fields: usize,
        stub_target: usize,
    },
    Array {
        target: usize,
        immediate: u32,
        length_slot: i32,
    },
}

pub(in crate::ir_lower) struct DeferredGenAllocTail {
    pub(in crate::ir_lower) map: Option<SafepointMapToken>,
    pub(in crate::ir_lower) slow: Vec<usize>,
    pub(in crate::ir_lower) slow_map_bytes: Vec<u8>,
    pub(in crate::ir_lower) ensure_frame_block: bool,
    pub(in crate::ir_lower) kind: DeferredGenAllocKind,
    pub(in crate::ir_lower) resume: usize,
    pub(in crate::ir_lower) exit_bci: usize,
    pub(in crate::ir_lower) node_pc: Option<usize>,
    pub(in crate::ir_lower) site_bci: usize,
}

pub(in crate::ir_lower) struct DeferredCheckCastTail {
    pub(in crate::ir_lower) slow: Vec<usize>,
    pub(in crate::ir_lower) slow_map_bytes: Vec<u8>,
    pub(in crate::ir_lower) ensure_frame_block: bool,
    pub(in crate::ir_lower) map: Option<SafepointMapToken>,
    pub(in crate::ir_lower) pending_shadow: Vec<i16>,
    pub(in crate::ir_lower) slot: i32,
    pub(in crate::ir_lower) result_forwarded: bool,
    pub(in crate::ir_lower) obj_slot: i32,
    pub(in crate::ir_lower) name_ptr: usize,
    pub(in crate::ir_lower) name_len: usize,
    pub(in crate::ir_lower) helper: usize,
    pub(in crate::ir_lower) resume: usize,
    pub(in crate::ir_lower) exit_bci: usize,
    pub(in crate::ir_lower) node_pc: Option<usize>,
    pub(in crate::ir_lower) site_bci: usize,
    pub(in crate::ir_lower) site_frame: Option<u32>,
}

pub(in crate::ir_lower) struct DeferredGetfieldTail {
    pub(in crate::ir_lower) receiver_gpr: u8,
    /// The site's `JZ` on the compact bit, absent for legacy-only sites.
    pub(in crate::ir_lower) legacy_patch: Option<usize>,
    /// Every guard edge of the site to the checked helper.
    pub(in crate::ir_lower) slow: Vec<usize>,
    /// The site's join, where the result is stored.
    pub(in crate::ir_lower) done: usize,
    pub(in crate::ir_lower) ref_node: bool,
    pub(in crate::ir_lower) type_tag: u8,
    pub(in crate::ir_lower) legacy_cell_off: i32,
    pub(in crate::ir_lower) node_ty: IrType,
    /// The receiver for the helper's second argument.
    pub(in crate::ir_lower) base_src: SelfCallArgSource,
    /// `getfield_index_arg(..)`, the helper's third argument.
    pub(in crate::ir_lower) arg2: u64,
    /// `cur_bci` / `cur_node_pc` at the site, for the exception exit.
    pub(in crate::ir_lower) site_bci: usize,
    pub(in crate::ir_lower) node_pc: Option<usize>,
    /// JIT round 25 item 2, wave 4: `(register, home)` of every lazy reference
    /// live at the site (`Lowerer::lazy_ref_pairs_here`); empty without one.
    pub(in crate::ir_lower) lazy_spills: Vec<(u8, i32)>,
    /// Selected primitive destination at the site, installed before rejoining.
    pub(in crate::ir_lower) result_gpr: Option<u8>,
    /// The implicit-null fault site of the access (its first receiver
    /// dereference). Its recovery pc is this tail's helper entry, registered
    /// in `page_poll_sites` when the tail is emitted
    /// (`Lowerer::emit_deferred_getfield_tails_with_fault_entries`).
    pub(in crate::ir_lower) implicit_nn_site: Option<usize>,
}

pub(in crate::ir_lower) enum DeferredZgcAllocKind {
    Array {
        plan: crate::runtime_lowering::InlineTlabArrayPlan,
        stub_target: usize,
    },
    New {
        plan: crate::runtime_lowering::InlineTlabPlan,
        stub_target: usize,
        slow_map_bytes: Vec<u8>,
        ensure_frame_block: bool,
    },
}

/// One direct self-call's cold half, recorded at the site and emitted after
/// the body by [`Lowerer::emit_deferred_self_call_tails`] (round 13 wave 8,
/// lane callcost3; proposal CC2-1). `entry_patch` is the site's forward
/// `Jcc rel32` operand, `resume` the hot-path offset the tail jumps back to,
/// `sp_id` the deferred safepoint id (`deferred_self_call_sp_id`), and `bci`
/// the site's method bci for the call-exception row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum DeferredSelfCallTail {
    /// The stack sample's slow side (`JBE` from the sample): the guard
    /// helper and its trip exit.
    StackGuard {
        entry_patch: usize,
        resume: usize,
        sp_id: Option<u32>,
        map: Option<SafepointMapToken>,
        copy_back: Option<(i32, i32, Vec<i16>)>,
        bci: usize,
    },
    /// The sentinel path after the `CALL` (`JO`): identity republish, the
    /// callee-deopt service, the re-check and the `dispatch_threw` peek.
    Sentinel {
        entry_patch: usize,
        resume: usize,
        sp_id: Option<u32>,
        map: Option<SafepointMapToken>,
        copy_back: Option<(i32, i32, Vec<i16>)>,
        identity_elided: bool,
        info_ptr: usize,
        args: Vec<(SelfCallArgSource, bool)>,
        bci: usize,
        /// JIT round 19 wave 4, lane throws4 (THROWS3-3,
        /// `CRATONVM_JIT_SELF_CALL_SENTINEL_LEAN`): the site's
        /// [`Lowerer::self_call_sentinel_lean_site`] answer.
        lean: bool,
    },
    /// Round 14 wave 3 (lane calls, proposal CC3-1 of
    /// `jit-r13-callcost3-proposals-RETIRED-20260929.md`): a merged direct cross call's cold
    /// side (`JO` after its one sentinel compare, entered with `RAX ==
    /// i64::MIN` after the site's republish and shadow reload): the
    /// callee-deopt service staging from the captured sources, then
    /// [`Lowerer::emit_call_return_sentinel_tail`]'s exit (`wide`: the
    /// `dispatch_threw` peek of a `long`/`float`/`double` result).
    CrossCallSentinel {
        map: Option<SafepointMapToken>,
        entry_patch: usize,
        resume: usize,
        info_ptr: usize,
        args: Vec<SelfCallArgSource>,
        wide: bool,
        bci: usize,
        /// THROWS4-1 (lane throws5): hand the service the lean bit.
        lean: bool,
        /// JIT round 25 wave 7 (W7-C): the site's hot path did not republish
        /// the frame record (`TailRepublishSink`), so the tail does it first.
        republish: bool,

        /// JIT round 21, lane argwin (OOM21-1): the site left its arguments
        /// to the callee; `args` is empty and the service is handed
        /// `crate::SERVICE_ARGS_OWNED_BY_CALLEE` and no buffer.
        owned: bool,
    },
}

/// Round 14 wave 3 (lane calls, CC3-1): `CRATONVM_JIT_IR_CROSS_CALL_COLD_TAILS`,
/// default ON. `0` keeps a merged direct cross call's callee-deopt service and
/// exception tail inline behind the taken `JNO` (the round-13 shape). Read per
/// site.
pub(in crate::ir_lower) fn ir_cross_call_cold_tails_enabled() -> bool {
    true
}

/// May the entry fast return skip a node of the two blocks it replaces?
/// Exactly the pure allowlist ([`op_cannot_deopt`]: no call, no trap, no
/// store, no safepoint), minus the OSR-entry shapes (they read the
/// interpreter's frame, which only an OSR entry supplies) and minus control
/// nodes, which are never data nodes of a block anyway.
pub(in crate::ir_lower) fn entry_fast_return_may_skip(op: &Op) -> bool {
    op_cannot_deopt(op)
        && !matches!(
            op,
            Op::OsrEntryFlag { .. }
                | Op::OsrLocal(_)
                | Op::OsrMemory
                // nio21 wave 3 L2: the caught throwable reads a word only a
                // call's exception pad writes (and the flag sits after a call).
                | Op::CallThrew { .. }
                // JIT round 19 wave 5 (lane handler5): the compiled handler
                // entry reads a word only its probe writes.
                | Op::CatchEntryFlag { .. }
                | Op::CaughtException
                | Op::If
                | Op::Switch { .. }
                | Op::Return
        )
}

/// Does the block CFG contain a cycle? An iterative three-colour DFS over
/// `successors` from every block (not only block 0: an unreachable cycle
/// still counts), total and panic-free. Round 12 wave 8 (lane callcost):
/// the dominated-sample elision is planned only for a loop-free body, where
/// no OSR entry can enter past a dominator.
pub(in crate::ir_lower) fn schedule_block_cfg_has_cycle(schedule: &Schedule) -> bool {
    const WHITE: u8 = 0;
    const GRAY: u8 = 1;
    const BLACK: u8 = 2;
    let n = schedule.blocks.len();
    let mut colour = vec![WHITE; n];
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for root in 0..n {
        if colour.get(root).copied() != Some(WHITE) {
            continue;
        }
        if let Some(c) = colour.get_mut(root) {
            *c = GRAY;
        }
        stack.push((root, 0));
        while let Some(&(b, next)) = stack.last() {
            let succ = schedule
                .blocks
                .get(b)
                .and_then(|blk| blk.successors.get(next))
                .copied();
            match succ {
                Some(s) => {
                    if let Some(top) = stack.last_mut() {
                        top.1 += 1;
                    }
                    match colour.get(s).copied() {
                        Some(GRAY) => return true,
                        Some(WHITE) => {
                            if let Some(c) = colour.get_mut(s) {
                                *c = GRAY;
                            }
                            stack.push((s, 0));
                        }
                        // BLACK, or a successor index past the list (which a
                        // well-formed schedule never has; ignored as the
                        // dominator fixpoint ignores it).
                        _ => {}
                    }
                }
                None => {
                    if let Some(c) = colour.get_mut(b) {
                        *c = BLACK;
                    }
                    stack.pop();
                }
            }
        }
    }
    false
}

pub(in crate::ir_lower) struct Lowerer<'a> {
    /// The compile's direct-helper table. Its `resolve_static_base` decides
    /// whether a `getstatic` loads its slot directly. `EMPTY` from
    /// `Lowerer::new`; `lower_inner_with_array_list` sets it.
    pub(in crate::ir_lower) direct_helpers: crate::DirectHelperTable,
    pub(in crate::ir_lower) graph: &'a Graph,
    pub(in crate::ir_lower) schedule: &'a Schedule,
    pub(in crate::ir_lower) buf: ExecutableBuffer,
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
    pub(in crate::ir_lower) node_slot: Vec<Option<NonZeroU32>>,
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
    pub(in crate::ir_lower) unallocated_slot_use: std::cell::Cell<bool>,
    /// Structured reason for the first latched failure (the `Bailout` form of
    /// `unallocated_slot_use`, plus any resource bailout raised from a legacy
    /// infallible accessor). Read and reported by `lower_inner`.
    pub(in crate::ir_lower) latched_bailout: std::cell::RefCell<Option<Bailout>>,
    /// Liveness-based frame-slot colouring for this method: which 8-byte spill
    /// slot each value lives in. Computed by [`plan_slots`] before the frame is
    /// sized, so `frame_size` budgets the *peak simultaneously live* value
    /// count rather than the node count. `alloc_slot_checked` reads it; it
    /// never bump-allocates.
    pub(in crate::ir_lower) slot_plan: &'a SlotPlan,
    /// gen r5w2/oomjit6: the facts that let a call site zero the reference
    /// homes nothing can read again (`CRATONVM_JIT_IR_CLEAR_DEAD_REF_SLOTS`).
    /// `None` -- clear nothing -- unless the opt-in is set and the method's
    /// bytecode was handed in; see the section above [`DeadRefSlotClear`].
    pub(in crate::ir_lower) dead_ref_clear: Option<DeadRefSlotClear>,
    /// gen r5w2/oomjit6: `Some((plan position, method bci, node))` exactly
    /// while an `Op::Call` is being lowered, so the GC points its lowering
    /// emits may clear dead reference homes. `None` everywhere else, which
    /// clears nothing. gen r5w5/oomjit9: a call inside a splice is armed too,
    /// with the bci of the enclosing invoke of this method's own code
    /// ([`resolve_spliced_pc`]); the node id keys
    /// [`DeadRefSlotClear::after_kept`]. gci: also while an armed allocation
    /// (i1/fr), an armed non-call GC point (i4/ir) or an armed back-edge
    /// poll's map (i5/ir, keyed by [`poll_site_key`], which names no node) is
    /// lowered.
    pub(in crate::ir_lower) dead_ref_clear_site: Option<(usize, usize, NodeId)>,
    /// gen r5w3/live7: the number of value inputs of the armed `Op::Call`
    /// (`inputs.len() - 2`), an upper bound on the argument-staging words its
    /// lowering writes (every staging store is `arg[i]` for `i < num_args`,
    /// read from `inputs[2 + i]`). Meaningful only while
    /// `dead_ref_clear_site` is `Some`.
    pub(in crate::ir_lower) dead_ref_clear_site_args: usize,
    /// gen r5w3/live7: emit the dead-home and dead-staging STORES of
    /// `CRATONVM_JIT_IR_CLEAR_DEAD_REF_SLOTS`. Separate from
    /// `dead_ref_clear.is_some()` since the facts are also built for the
    /// store-free claim below; `false` whenever the facts are absent.
    pub(in crate::ir_lower) dead_ref_clear_stores: bool,
    /// gen r5w3/live7 (`CRATONVM_JIT_PRECISE_FRAME_LIVENESS`): publish, at every
    /// armed call-site safepoint, which reference homes and staging words hold
    /// a LIVE reference ([`crate::FrameLivenessTable`]), and leave the dead
    /// homes out of the map and the shadow push. `false` whenever the facts
    /// are absent. See the section above [`DeadRefSlotClear`].
    pub(in crate::ir_lower) frame_liveness_on: bool,
    /// gen r5w5/oomjit9 (default on; `CRATONVM_JIT_IR_DEAD_HOME_CLEARS=0` is
    /// the kill switch): the frame words each armed `Op::Call` zeroes at its
    /// FIRST byte -- the reference homes dead at the call that may still hold
    /// a value written since they were last zeroed, and the argument-staging
    /// words an earlier call may have left a reference in. Planned before
    /// emission by [`Self::plan_dead_home_clears`]; empty (zero nothing)
    /// under the kill switch, without the method's bytecode, or while a
    /// debugger may read locals. See the section above
    /// [`ir_dead_home_clears_enabled`].
    pub(in crate::ir_lower) dead_home_clears: HashMap<NodeId, Vec<i32>>,
    /// `CRATONVM_JIT_IR_LOOP_ENTRY_DEAD_HOME_CLEARS` (TLGET21-3 close): per
    /// block, the reference homes [`Self::plan_dead_home_clears`] took off
    /// the armed sites of a loop and zeroes once at the END of that loop's
    /// entry block instead ([`move_loop_entry_dead_home_clears`]). Empty off
    /// the switch.
    pub(in crate::ir_lower) loop_entry_dead_home_clears: HashMap<usize, Vec<i32>>,
    /// A converged stale-home plan on a fresh zeroed frame replaces the
    /// dense per-safepoint home stores. Staging clears retain their own gate.
    /// False if the incremental proof or entry initialization is absent.
    pub(in crate::ir_lower) dead_home_clears_complete: bool,
    /// hga (`CRATONVM_JIT_IR_PREFIX_SLOW_PATH_CLEARS`, default on): a call
    /// that may take an inline prefix zeroes its [`Self::dead_home_clears`]
    /// on its call path, after the prefixes, instead of at its first byte.
    /// Set before [`Self::plan_dead_home_clears`], which reads it.
    pub(in crate::ir_lower) prefix_slow_path_clears: bool,
    /// gci i1/fr (`CRATONVM_JIT_IR_ALLOC_SITE_CLEARS`, opt-in): an `Op::New` /
    /// `Op::NewArray` is an armed site too ([`dead_ref_clear_arms`]). Set
    /// before the dead-reference facts are planned.
    pub(in crate::ir_lower) alloc_site_clears: bool,
    /// gci i4/ir (`CRATONVM_JIT_IR_OTHER_GC_POINT_CLEARS`, opt-in): the
    /// control-pinned non-call GC-point ops are armed sites too
    /// ([`dead_ref_clear_arms_node`]). Set before the dead-reference facts
    /// are planned.
    pub(in crate::ir_lower) other_site_clears: bool,
    /// gci i5/ir (`CRATONVM_JIT_IR_POLL_GC_POINT_CLEARS`, opt-in): each
    /// block's edge position ([`block_edge_positions`]) while the loop
    /// back-edge polls are armed sites ([`Self::poll_dead_ref_clear_site`]);
    /// empty -- no poll armed -- off the switch, without the dead-reference
    /// facts, or when the positions could not be re-derived.
    pub(in crate::ir_lower) poll_edge_pos: Vec<usize>,
    /// gci i5/ir: `true` exactly while an armed poll's map is emitted, so
    /// [`Self::emit_dead_ref_slot_clears`] stores even where the planned
    /// first-byte clears are complete (no planned clear runs before a poll).
    pub(in crate::ir_lower) poll_site_dense: bool,
    /// gci i3/ir (`CRATONVM_JIT_IR_REF_PARAM_HOME_ZERO`, default ON): the
    /// reference `Op::Param` nodes whose prologue home is zeroed right after
    /// their copy into their own colour ([`ref_param_home_zero_candidates`],
    /// [`Self::zero_ref_param_home_after_copy`]). Ascending; empty off the
    /// switch or without the dead-reference facts.
    pub(in crate::ir_lower) ref_param_home_zero: Vec<NodeId>,
    /// gkr: `(node, colour home, prologue word)` of every parameter
    /// `plan_lazy_ref_param_home_zero` took ([`Self::emit_dead_home_clear`]).
    pub(in crate::ir_lower) lazy_param_homes: Vec<(NodeId, i32, i32)>,
    /// JIT round 25 item 2 (`CRATONVM_JIT_IR_SHADOW_LIVE_ACROSS_CALL`,
    /// default ON): every [`ref_param_home_zero_candidates`] parameter as `(node,
    /// prologue offset)`, taken BEFORE the lazy plan narrows
    /// [`Self::ref_param_home_zero`]. Read by
    /// [`Self::shadow_live_param_home_drops`]; empty off the switch.
    pub(in crate::ir_lower) shadow_live_param_homes: Vec<(NodeId, i16)>,
    /// r25 item 2: `CRATONVM_DBG_JITC`, read once with the switch above, for
    /// the per-site `[ir-shadow-live]` line.
    pub(in crate::ir_lower) shadow_live_dbg: bool,
    /// JIT round 25 item 2, wave 3 (`CRATONVM_JIT_IR_SHADOW_FRAME_BLOCK_LAZY_CLEAR`):
    /// may this attempt plan the lazy clear? Set by `lower_inner_attempt`
    /// (never on the keep-everything retry).
    pub(in crate::ir_lower) block_lazy_clear_allowed: bool,
    /// The frame-block words the prologue does not clear (ascending), each
    /// written on every path before every GC point but the entry poll, whose
    /// slow path zeroes them ([`Self::plan_block_lazy_clear`]). Empty: none.
    pub(in crate::ir_lower) block_lazy_clear: Vec<i16>,
    /// The definitions those writes rest on, as `(node, home offset)`;
    /// checked against the emission by [`Self::verify_block_lazy_clear_writes`].
    pub(in crate::ir_lower) block_lazy_writers: Vec<(NodeId, i32)>,
    /// Per node, the offset of its own home write `store_rax` emitted (0:
    /// none seen). Sized only while a lazy-clear plan exists.
    pub(in crate::ir_lower) own_home_written: Vec<i32>,
    /// JIT round 25 item 2, wave 4 (`CRATONVM_JIT_IR_LAZY_REF_HOMES`): the
    /// reference values whose definition does not store their home
    /// ([`Self::plan_lazy_ref_homes`]); per node, empty without a plan.
    pub(in crate::ir_lower) lazy_ref_home: Vec<bool>,
    /// Per hooked node (a field load, or a no-call array op), the lazy values
    /// live across it: spilled before its helper call and reloaded after.
    pub(in crate::ir_lower) lazy_ref_sites: HashMap<NodeId, Vec<NodeId>>,
    /// Code spans `[start, end)` of every hooked helper call (spill .. reload),
    /// for the no-unhooked-call check of [`Self::lazy_ref_after_node`].
    pub(in crate::ir_lower) lazy_hook_regions: Vec<(usize, usize)>,
    /// Census for the `[ir-lazy-ref]` line: values planned, spill stores
    /// emitted, reference definition home stores emitted.
    pub(in crate::ir_lower) lazy_ref_planned: usize,
    pub(in crate::ir_lower) lazy_ref_spill_stores: usize,
    pub(in crate::ir_lower) ref_def_home_stores: usize,
    /// The `ir-home-keep` census (`CRATONVM_DBG_JITC`): why each emitted
    /// definition home store was kept, by [`HomeKeep`] class.
    pub(in crate::ir_lower) home_keep_census: [usize; HOME_KEEP_CLASSES],
    /// RB25-1 step 2: jump-entered flags whose `XOR; store` was not emitted
    /// because every reader is an `If` that folds (`jump_entered_flag_store_unread`).
    pub(in crate::ir_lower) flag_store_elided: Vec<bool>,
    /// The same, weighted by the storing block's estimated frequency.
    pub(in crate::ir_lower) home_keep_freq: [f64; HOME_KEEP_CLASSES],
    /// The same by `class:producer->consumer#operand` shape.
    pub(in crate::ir_lower) home_keep_shapes: HashMap<String, (usize, f64, NodeId)>,
    /// Wave 5 census: otherwise-eligible values blocked only by real calls in
    /// their range, and by anything else (a store, an allocation, a poll ...).
    pub(in crate::ir_lower) lazy_ref_blocked_by_calls: usize,
    pub(in crate::ir_lower) lazy_ref_blocked_other: usize,
    /// hga: the clears of the `Op::Call` being lowered, until
    /// [`Self::emit_pending_call_clears`] emits them after its prefixes.
    pub(in crate::ir_lower) pending_call_clears: Option<Vec<i32>>,
    /// hga: the dense staging clear (`emit_dead_arg_stage_clears`) of the
    /// `Op::Call` being lowered, held the same way.
    pub(in crate::ir_lower) pending_arg_stage_clears: bool,
    /// JIT round 20 wave 17, lane gcDead (I-9,
    /// `CRATONVM_JIT_IR_ALLOC_DEAD_HOME_CLEAR`, default on): `Op::New` /
    /// `Op::NewArray` are armed sites too ([`Self::dead_ref_clear_site_for`]),
    /// their clears held for the allocation's slow path. Set before
    /// [`Self::plan_dead_home_clears`], which turns it off again when its
    /// stale-set union does not converge. See the section above
    /// [`ir_alloc_dead_home_clear_enabled`].
    pub(in crate::ir_lower) alloc_dead_home_clear: bool,
    /// JIT round 21, lane oom (`CRATONVM_JIT_IR_HELPER_SITE_DEAD_HOME_CLEAR`,
    /// default on): with [`Self::alloc_dead_home_clear`], the GC points I-9
    /// left unarmed are armed too -- the post-init `Op::New` bump (every ZGC
    /// `new`), `Op::NewCp` and `Op::MonitorEnter`'s slow path. See the section
    /// above [`ir_helper_site_dead_home_clear_enabled`].
    pub(in crate::ir_lower) helper_site_dead_home_clear: bool,
    /// JIT round 21, lane argwin (OOM21-1, `CRATONVM_JIT_IR_CALLEE_OWNED_ARGS`,
    /// default ON): the method's own bytecode and exception table, handed over
    /// only under the switch and only while no debugger may read locals, for
    /// the owned-arguments admission of a direct call
    /// ([`Self::ir_direct_call_args_owned_by_callee`]). `None`: no site is
    /// ever owned. See the section above [`ir_callee_owned_args_enabled`].
    pub(in crate::ir_lower) owned_args_bytecode: Option<DeadRefClearBytecode<'a>>,
    /// ARGWIN21-7: the spliced raw pcs the keep set read as chain-resumed
    /// ([`DeadRefSlotClear::chain_only_pcs`]); a deopt there that resolves
    /// flat refuses the attempt ([`Self::refuse_flat_at_chain_only_pc`]).
    pub(in crate::ir_lower) keep_set_chain_only_pcs: std::collections::HashSet<usize>,
    /// OOM21-1: the direct cross call being lowered passes
    /// [`crate::SERVICE_ARGS_OWNED_BY_CALLEE`] to the callee-deopt service and
    /// stages nothing for it. Set by the `Op::Call` arm around
    /// [`Self::emit_direct_cross_call`] only, `false` everywhere else.
    pub(in crate::ir_lower) direct_call_args_owned: bool,
    /// OOM21-1 step 2 (`CRATONVM_JIT_IR_CALLEE_ARG_WINDOW`, default ON): the
    /// frame homes of the argument-only reference arguments of the owned
    /// direct call being lowered, zeroed (with their shadow copies) between
    /// the argument marshal and the `CALL`
    /// ([`Self::emit_owned_call_arg_window_clears`]). Empty everywhere else.
    pub(in crate::ir_lower) direct_call_arg_window: Vec<i32>,
    /// gkr (OOM21-2): the argument-only homes of the REFUSED direct call being
    /// lowered (`argument_only_homes`), recorded with its return address
    /// ([`Self::note_ir_service_arg_copies`]). Empty everywhere else.
    pub(in crate::ir_lower) direct_call_service_copies: Vec<i32>,
    /// gkr (OOM21-2): `(return offset, homes)` per such call, for
    /// `CompiledMethod::service_arg_copies`.
    pub(in crate::ir_lower) service_arg_copies: Vec<(u32, Box<[i16]>)>,
    /// JIT round 20 wave 21 (lane pollroot,
    /// `CRATONVM_JIT_IR_BACK_EDGE_DEAD_HOME_CLEAR`, default on): a loop
    /// back-edge poll zeroes the home of every `Ref` value dead there
    /// ([`back_edge_dead_ref_homes`]), not only the spliced referent reads.
    /// See [`Lowerer::emit_back_edge_dead_home_clears`].
    pub(in crate::ir_lower) back_edge_dead_home_clear: bool,
    /// JIT round 20 wave 17, lane gcDead: [`Self::plan_dead_home_clears`]
    /// converged, so every armed site zeroes (before its GC point) each dead
    /// home that may be stale. [`Self::precise_liveness_claim`] claims nothing
    /// without it: see
    /// `docs/feature-designs/jit-ir-optimizations.md`.
    pub(in crate::ir_lower) dead_home_plan_converged: bool,
    /// gen r5w3/live7: the words every liveness record speaks for (sorted),
    /// computed at the first claiming safepoint. Empty until then.
    pub(in crate::ir_lower) frame_liveness_covered: Vec<i16>,
    /// gen r5w3/live7: one record per claiming safepoint, in id order.
    pub(in crate::ir_lower) frame_liveness_records: Vec<crate::SafepointFrameLiveness>,
    /// gen r5w3/live7: two records disagreed about the covered set; the
    /// finalize then publishes no table (`take_frame_liveness`).
    pub(in crate::ir_lower) frame_liveness_poisoned: bool,
    /// Highest spill offset any already-emitted node's slot reaches
    /// (exclusive), i.e. `max(offset + 8)` over every `alloc_slot` so far.
    ///
    /// Was `next_spill`, the bump cursor, whose value happened to mean the same
    /// thing while slots were handed out in emission order. Under colouring the
    /// cursor is gone but the *watermark* is still exactly what a safepoint's
    /// `live_frame_hi` needs: words above it have never been written by this
    /// frame, so the collector's band scan may skip them.
    pub(in crate::ir_lower) spill_high_water: i32,
    /// Maps block index → native code offset (for branch patching).
    pub(in crate::ir_lower) block_offsets: Vec<usize>,
    /// Forward branch patches: (native_offset_of_rel32, target_block_idx).
    pub(in crate::ir_lower) branch_patches: Vec<(usize, usize)>,
    /// Jump tables owed by the `Op::Switch` sites lowered so far, in emission
    /// order. Each is laid out by [`Lowerer::emit_switch_tables`] AFTER the
    /// last block, never inline in the instruction stream — see that function.
    pub(in crate::ir_lower) switch_tables: Vec<SwitchTablePlan>,
    /// Jump-table entry patches: `(native offset of the i32 entry, native
    /// offset of its table's base, target block)`. Resolved by
    /// [`Lowerer::patch_branches`] once every block has an offset, to a
    /// **table-base-relative** displacement — the same convention the
    /// single-pass backend's `jump_table_patches` uses, and for the same
    /// reason: the dispatch adds the table base back, so the stored value is
    /// position-independent within the code buffer.
    pub(in crate::ir_lower) switch_table_patches: Vec<(usize, usize, usize)>,
    /// Number of parameter slots.
    pub(in crate::ir_lower) num_params: usize,
    /// Number of local variable slots.
    /// `max_locals`. Read by the prologue's unset-local zeroing.
    ///
    /// JIT round 25 wave 2 (`CRATONVM_JIT_IR_FRAME_PARAM_LOCALS_ONLY`, default ON since j27 2026-10-28):
    /// the Java-local words the FRAME reserves, which under the switch is the
    /// parameter words only ([`ir_frame_locals`]); every other local of this
    /// tier lives in a coloured home and never in a local word.
    pub(in crate::ir_lower) num_locals: usize,
    /// Frame size (aligned).
    pub(in crate::ir_lower) frame_size: i32,
    /// real-frame-deopt: bytecode pc → earliest native code offset emitted
    /// for that bci. Populated as nodes are lowered; used to anchor each
    /// safepoint snapshot to a native offset for `DeoptimizationPoint`.
    pub(in crate::ir_lower) bci_native: HashMap<usize, usize>,
    /// JIT round 17 wave 3 (lane irlower; item 1 of
    /// `r17-monitor2-by-bci-readers-that-stay-by-bci-FIXED-20261005.md`): per bci,
    /// the blocks that define a value some by-bci snapshot there names
    /// ([`bci_anchor_value_blocks`]). `None` with
    /// `CRATONVM_JIT_IR_BCI_ANCHOR_DOMINATED=0`. See
    /// [`Lowerer::note_bci_anchor`].
    pub(in crate::ir_lower) bci_anchor_value_blocks: Option<HashMap<usize, Vec<usize>>>,
    /// Whether [`Self::bci_native`]'s anchor of a bci lies in a block every
    /// one of those blocks dominates. Empty with the switch off.
    pub(in crate::ir_lower) bci_native_dominated: HashMap<usize, bool>,
    /// `Graph::safepoints` index → earliest native offset emitted for a node
    /// that names it through [`crate::ir::Node::frame_snapshot`].
    ///
    /// The per-COPY counterpart of [`Self::bci_native`]. A cloned loop body has
    /// `trip` nodes at each of its bcis, so `bci_native` holds one offset for
    /// all of them (copy 0's) and the other copies have no anchor of their own.
    /// A snapshot that belongs to a specific copy anchors here instead, at an
    /// offset inside that copy's code.
    ///
    /// EMPTY on every compile that does not unroll over a named body, which
    /// makes `build_deopt_points` fall through to `bci_native` for every
    /// snapshot exactly as it always did.
    pub(in crate::ir_lower) snapshot_native: HashMap<u32, usize>,
    /// `Graph::safepoints` index the node currently being lowered names as its
    /// own frame, or `None` when it names none — which is every node of every
    /// graph that did not unroll over a named body.
    ///
    /// Read by [`Self::resolve_frame_state_for_site`], which is what every
    /// guard's boxed deopt point goes through. Set beside [`Self::cur_node_pc`]
    /// and reset the same way, for the same reason: a stale copy identity
    /// attributed to a later node is a frame from the wrong iteration, and this
    /// area's rule is that no frame is better than a wrong one.
    pub(in crate::ir_lower) cur_node_frame: Option<u32>,
    /// Bytecode pc of the node currently being lowered — the throw-site bci
    /// every exceptional exit emitted while lowering it belongs to.
    ///
    /// Set from `Node::bytecode_pc` at the top of `lower_data_node` /
    /// `lower_terminator` (a node with no pc keeps the previous value rather
    /// than resetting to 0: the pc-less nodes are the scheduler's own control
    /// glue, which emits no exceptional exit of its own). Read by
    /// [`Self::push_call_exc_patch`].
    pub(in crate::ir_lower) cur_bci: usize,
    /// The bytecode pc of the node being lowered, or `None` when that node
    /// carried none.
    ///
    /// Distinct from [`Self::cur_bci`], which deliberately KEEPS the previous
    /// value for a pc-less node so an exceptional exit still has a throw-site
    /// bci. That persistence is right for a line and wrong for an inline
    /// chain: attributing the previous node's splice nesting to a call emitted
    /// under scheduler glue would name callees that are not on the stack, and
    /// this area's rule is that no frame is better than a wrong one. Reset to
    /// `None` on every pc-less node, so
    /// [`Self::note_inline_frame_return_site`] fails closed instead.
    pub(in crate::ir_lower) cur_node_pc: Option<usize>,
    /// real-frame-deopt: native offsets of `JMP rel32` instructions emitted by
    /// failed guards that must be patched to jump to the shared deopt stub.
    pub(in crate::ir_lower) deopt_stub_patches: Vec<usize>,
    /// Out-of-line deopt pads not yet emitted: `(rel32 patch of the guard's
    /// failing Jcc, the point pointer the pad loads into DEOPT_ARG0)`. Filled
    /// by `emit_deopt_jcc_to_pad`, drained after the body by
    /// `emit_deopt_pads`.
    pub(in crate::ir_lower) deopt_pads: Vec<(usize, u64)>,
    /// `true` and `ir_publish_at_def_enabled()`, read ONCE
    /// per compile (round 9). Both were asked on the hottest paths in this
    /// file — every operand read (`gp_load_value`) and every home store
    /// (`publish_def_at_store`) — and each ask is a flag-snapshot lookup that
    /// clones an `OsString`. Neither can change mid-compile.
    pub(in crate::ir_lower) const_imm_on: bool,
    pub(in crate::ir_lower) publish_at_def_on: bool,
    /// The per-NODE and per-EDGE gates, read once per compile like the two
    /// above (round 11 wave 2, lane lower; `r11-irlower-flag-reads-in-per-
    /// node-paths`). Each was asked up to three times per ALU node, once per
    /// field site, once per block edge or phi copy, and each ask is a declared-
    /// flag lookup that clones an `OsString`. A test's `with_thread_overrides`
    /// or force guard is in effect when the lowerer is built, so latching them
    /// here keeps that property.
    pub(in crate::ir_lower) alu_imm_on: bool,
    pub(in crate::ir_lower) fused_branch_on: bool,
    pub(in crate::ir_lower) fallthrough_on: bool,
    pub(in crate::ir_lower) phi_copy_regs_on: bool,
    pub(in crate::ir_lower) receiver_cse_on: bool,
    pub(in crate::ir_lower) ref_residency_on: bool,
    /// `CRATONVM_JIT_INLINE_PRIM_PUTFIELD` (default ON, `=0` kill switch) — the
    /// single-pass tier's switch for its inline primitive store, honoured by
    /// this tier's twin ([`Self::emit_inline_prim_putfield`]) so one switch
    /// reverts both.
    pub(in crate::ir_lower) inline_prim_putfield_on: bool,
    /// [`Self::reloc_emit_enabled`] and `CRATONVM_DBG_IR_RELOC`, read once per
    /// compile: both were asked at every GC point (`emit_safepoint_map`).
    pub(in crate::ir_lower) reloc_emit_on: bool,
    pub(in crate::ir_lower) dbg_ir_reloc_on: bool,
    /// `runtime_lowering::volatile_load_fence_enabled`, read once per compile
    /// (it was asked at every volatile `getstatic`).
    pub(in crate::ir_lower) volatile_load_fence_on: bool,
    /// Ids of every `IrType::Ref` node, ascending. `emit_safepoint_map` walks
    /// these instead of the whole graph at each GC point: the type filter is
    /// static, and ascending id order keeps the published slot order (and so
    /// the emitted bytes) exactly what the whole-graph walk produced.
    pub(in crate::ir_lower) ref_node_ids: Vec<usize>,
    /// `emit_safepoint_map`'s dedup bitset, kept between GC points so each
    /// point clears and reuses one allocation instead of making a new one
    /// (round 11 wave 5, `r11-irlower-safepoint-map-compile-cost`).
    pub(in crate::ir_lower) sp_map_seen: Vec<u64>,
    /// `CRATONVM_JIT_IR_CARRY_RCX_TWIN` (default ON, `=0` restores the
    /// `MOV RCX, RAX`), read once per compile like the two above. Round 9
    /// wave 11 (irl11); see [`RcxTwin`].
    pub(in crate::ir_lower) rcx_twin_on: bool,
    /// `CRATONVM_DBG_IR_POISON_INT_HIGH` (default OFF), read once per compile:
    /// the int upper-half poisoning stress mode (round 12 wave 2, lane irback,
    /// proposal R12-1). See [`Self::emit_poison_int_high`].
    pub(in crate::ir_lower) poison_int_high_on: bool,
    /// The RCX twin the CURRENT definition's arm offered, if any. Taken (and
    /// therefore cleared) by every `store_rax`; see [`Self::apply_rcx_twin`].
    pub(in crate::ir_lower) rcx_twin: Option<RcxTwin>,
    /// ENGAGEMENT: RCX carries installed by a twin instead of a `MOV RCX, RAX`.
    pub(in crate::ir_lower) rcx_twins: usize,
    /// Bytecode indices at which this body can actually transfer to the
    /// interpreter — the resume bci of every guard [`Self::emit_deopt_unless`]
    /// planted, and the throw-site bci of every call-exception patch.
    ///
    /// # What it is for
    ///
    /// `IrBuilder` records a `SafepointSnapshot` at EVERY bytecode index, and
    /// `build_deopt_points` turns each one that got a native offset into a
    /// `DeoptimizationPoint` carrying a fully-resolved frame state. Measured on
    /// one 52-node graph: 48 snapshots, 17 points, **9 970 bytes of deopt
    /// metadata against 1 207 bytes of code** — and across a whole run, 5.0 MB
    /// of metadata for 630 KB of code, a factor of eight.
    ///
    /// Most of it describes program points nothing can arrive at. A guard's own
    /// deopt does not read this vector at all — `emit_deopt_unless` boxes its
    /// own point and passes the pointer in `DEOPT_ARG0` — so what the vector
    /// actually serves is the by-bci PROVENANCE checks on the resume path
    /// ("is this bci a recorded deopt point of this artifact"). A bci that
    /// cannot transfer is never asked about.
    ///
    /// Recorded at the two emission sites rather than predicted from the graph,
    /// for the reason `ir_drop_unreachable_homes_enabled` gives about its own
    /// prediction: a classification that decides what to DROP must be checked
    /// against the emission that actually happened, not trusted.
    pub(in crate::ir_lower) transfer_bcis: std::collections::HashSet<usize>,
    /// JIT r20 wave 19 (lane yLoopPred, LOOPOPT20-6): the subset of
    /// [`Self::transfer_bcis`] at which a transfer reads the FLAT snapshot of
    /// that bci (the by-bci scan): every call / pad / re-check exit, a guard
    /// whose node names no frame there, a chain point. A transfer through the
    /// node's own named frame (`cur_node_frame` at that bci) is not in it.
    /// The pure-body unroll's net reads it under
    /// `CRATONVM_JIT_IR_UNROLL_NAMED_FRAME_PCS` (`ir_optimize::unroll_named_frame_pcs_enabled`).
    pub(in crate::ir_lower) by_bci_transfer_bcis: std::collections::HashSet<usize>,
    /// Census only (`CRATONVM_DBG_IR_COMPILES`): the first site kind that read the flat
    /// snapshot at each bci of [`Self::by_bci_transfer_bcis`].
    pub(in crate::ir_lower) by_bci_transfer_why: std::collections::HashMap<usize, String>,
    /// The subset of [`Self::by_bci_transfer_bcis`] whose sites resume through the by-bci
    /// TABLE point of that bci (see `note_flat_transfer`).
    pub(in crate::ir_lower) table_transfer_bcis: std::collections::HashSet<usize>,
    /// JIT r22 LOOPS22-2 (`CRATONVM_JIT_IR_LOOP_RETIRE_SPLICED`): the raw
    /// combined pcs at which a transfer published a CHAIN snapshot
    /// ([`Self::splice_chain_for_site`]'s successful answer). The chain twin
    /// of [`Self::by_bci_transfer_bcis`]: a retired spliced loop's net
    /// (`ir_optimize::StrandedFrames::chain`) is checked against it.
    pub(in crate::ir_lower) chain_transfer_pcs: std::collections::HashSet<usize>,
    /// real-frame-deopt: boxed deopt points whose stable addresses are baked
    /// as imm64 into guard code. Moved into the `CompiledMethod` so the code's
    /// raw pointers stay valid for the method's (retained) lifetime.
    pub(in crate::ir_lower) deopt_boxes: Vec<Box<DeoptimizationPoint>>,
    pub(in crate::ir_lower) vthread_boundaries_emitted: Vec<u32>,
    /// JIT round 19 wave 3 (lane eager3, EAGER2-4,
    /// `CRATONVM_JIT_TRAP_CAUSE_ON_POINT`): `(point address, TrapCause code)`
    /// for every `Op::Guard` point whose pc the builder recorded a cause for
    /// (`Graph::trap_causes`). Moved into `CompiledMethod::trap_cause_points`.
    pub(in crate::ir_lower) trap_cause_points: Vec<(u64, u8)>,
    // ── Gap B: Op::Call (invokestatic via the dispatch helper) ───────────
    /// Address of the `jit_invoke_dispatch` runtime helper (baked into each
    /// `Op::Call` site as `MOV RAX,imm64 ; CALL RAX`). 0 if no calls.
    pub(in crate::ir_lower) invoke_dispatch: usize,
    pub(in crate::ir_lower) lambda_int_to_double: usize,
    /// Address of the `jit_dispatch_threw` peek helper. Baked into a `J`/`D`
    /// (long/double) call site's post-invoke check on the rare `RAX == i64::MIN`
    /// branch to disambiguate a genuine callee exception/deopt from a legitimate
    /// `Long.MIN_VALUE` return (see `lower_call`'s sentinel sequence). 0 if no
    /// calls / not wired (int/ref/void sites never consult it).
    pub(in crate::ir_lower) dispatch_threw: usize,
    /// Catchable native-stack overflow guard for direct self-recursion.
    pub(in crate::ir_lower) self_call_stack_guard: usize,
    /// IR FP tier (Slice A) — address of the `jit_frem` / `jit_drem` runtime
    /// helpers (`extern "C" fn(f32,f32)->f32` / `fn(f64,f64)->f64`). Baked into
    /// an `Op::Rem` Float/Double site as `MOV RAX,imm64 ; CALL RAX` with the two
    /// operands already in XMM0/XMM1 and the result read back from XMM0.
    pub(in crate::ir_lower) frem: usize,
    pub(in crate::ir_lower) drem: usize,
    /// Address of the checked `jit_getfield` helper. When present, `Op::Load`
    /// instance-field reads route through it so receivers are validated against
    /// the live heap before any object-header dereference.
    ///
    /// It is also the only compact-layout-correct `Op::Load` lowering: the
    /// inline fallback derives `HEADER_SIZE + field_index*SLOT_SIZE`, which a
    /// compact object does not obey. See [`compact_field_lowering_available`].
    pub(in crate::ir_lower) getfield: usize,
    /// Address of the `jit_putfield_int` helper — the compact-layout-correct
    /// `Op::Store` lowering, for the same reason as `getfield` above. The
    /// inline store is kept for the legacy (uniform-slot) layout, where it
    /// avoids a call per field write.
    pub(in crate::ir_lower) putfield_int: usize,
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
    pub(in crate::ir_lower) putfield_object: usize,
    /// `jit_aastore(vm_ptr, array_ptr, index, val)` -- the reference-array
    /// store, covariance check and both barriers included. See the
    /// `Op::ArrayStore(MemKind::Ref)` arm for the three obligations its call
    /// site owes.
    pub(in crate::ir_lower) aastore: usize,
    /// `jit_aastore_type_check(vm_ptr, array_ptr, val) -> 0 | i64::MIN` — the
    /// JVMS 6.5 covariance rule alone, with a DEFINED return. The `aastore`
    /// arm calls it before `jit_aastore` so a refusal leaves the body through
    /// the shared exception epilogue instead of by `jit_aastore`'s
    /// pending-signal channel, which the body never looked at (round 9 wave 2).
    /// `0` (a partial helper table) keeps the old single-call shape.
    pub(in crate::ir_lower) aastore_type_check: usize,
    /// zt t19/zb2: `jit_aaload(vm_ptr, array_ptr, index) -> element | i64::MIN`,
    /// the barriered reference-element read. Called by the `ArrayLoad(Ref)`
    /// arm only while the ZGC load barrier is armed
    /// (`zgc_read_barrier_blocks_inline_fields`); `0` refuses such a site.
    pub(in crate::ir_lower) aaload: usize,
    /// Which array accesses have a PROVEN-unnecessary null or bounds check.
    /// Empty (every answer `false`) when `CRATONVM_JIT_IR_CHECK_ELIM=0`, which
    /// is byte-for-byte the emission before the pass existed.
    pub(in crate::ir_lower) check_elision: crate::ir_check_elim::CheckElision,
    /// COV-03 — `jit_putfield_{long,float,double}`, the compact-layout-correct
    /// wide-field stores. Same `(obj_ptr, field_index, bits)` ABI as
    /// `putfield_int`, no context argument; the value rides a GPR as raw bits
    /// (`f32::to_bits` zero-extended / `f64::to_bits`), which is exactly how the
    /// FP frame slots already hold it.
    pub(in crate::ir_lower) putfield_long: usize,
    pub(in crate::ir_lower) putfield_float: usize,
    pub(in crate::ir_lower) putfield_double: usize,
    /// Compact-layout/TLAB-aware object allocation helper. Live `Op::New`
    /// nodes use the same shared runtime-lowering stub as the baseline tier.
    pub(in crate::ir_lower) new_object: usize,
    /// TLAB geometry and the post-init helper, for the inline bump that
    /// `Op::New` takes instead of the stub. All three are wired together or
    /// not at all; `emit_inline_tlab_new_ir` declines on any zero.
    pub(in crate::ir_lower) tlab_cursor_off: i32,
    pub(in crate::ir_lower) tlab_end_off: i32,
    pub(in crate::ir_lower) tlab_post_init: usize,
    /// JIT r20 wave 11 (lane pAlloc, `CRATONVM_JIT_IR_INLINE_TLAB_TLS_THREAD`):
    /// how many inline TLAB plans this body built with the thread read through
    /// the `JIT_THREAD` mirror / from the frame slot. Census only, for the
    /// lean-body line in `finish_lazy_thread_fetch`.
    pub(in crate::ir_lower) tlab_tls_thread_plans: u32,
    pub(in crate::ir_lower) tlab_frame_thread_plans: u32,
    /// cov-06 — `jit_newarray(vm, atype, length) -> array | 0`. The lowering
    /// for a PRIMITIVE `Op::NewArray` (`element_type != 0`); zero-fill, GC
    /// retry and the zero-on-failure convention (negative length OR OOM) are
    /// the same as `new_object`'s, and this is the identical helper the
    /// single-pass backend's 0xbc arm calls.
    pub(in crate::ir_lower) newarray: usize,
    /// cov-06 — `jit_anewarray_object(vm, component_class_id, length) -> array
    /// | 0`. The lowering for a REFERENCE `Op::NewArray` (`element_type ==
    /// 0`) — same zero-on-failure convention, same helper the single-pass
    /// backend's 0xbd arm calls.
    pub(in crate::ir_lower) anewarray_object: usize,
    /// Round 16 (lane matrix): `jit_multianewarray_n(vm, site, ndims,
    /// dims_ptr) -> array | 0` -- the helper the single-pass `0xc5` arm calls,
    /// and the lowering of [`Op::MultiNewArray`]. `0` (a synthetic table)
    /// refuses the op.
    pub(in crate::ir_lower) multianewarray_n: usize,
    /// nio21 wave 15 (lane L3): `jit_new_object_cp(vm, holder, cp_idx) -> obj
    /// | 0` -- the helper the single-pass `0xbb` arm calls for a deferred
    /// site, and the lowering of [`Op::NewCp`]. `0` (a synthetic table)
    /// refuses the op (`lower_inner`'s helper-presence list).
    pub(in crate::ir_lower) new_object_cp: usize,
    pub(in crate::ir_lower) monitor_enter: usize,
    pub(in crate::ir_lower) monitor_exit: usize,
    /// `helpers.monitor_block_offset_in_thread`: where the per-thread
    /// `JitMonitorBlock` sits in `JvmThread`, `0` = no inline thin-lock path
    /// (see [`Self::emit_monitor_fast_path`]).
    pub(in crate::ir_lower) monitor_block_off: usize,
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
    pub(in crate::ir_lower) ldc_string_cp: usize,
    pub(in crate::ir_lower) ldc_class_cp: usize,
    pub(in crate::ir_lower) getstatic: usize,
    /// `jit_putstatic_{int,long,float,double,object}(vm, class_id,
    /// field_index, value) -> 0 | i64::MIN`, in that order — the helpers an
    /// `Op::StoreStatic` always calls (the single-pass `0xb3` arm's). OptionalPtr
    /// like `getstatic`; `lower_inner` refuses a graph needing an absent one.
    pub(in crate::ir_lower) putstatic: [usize; 5],
    /// cov-05. `jit_instanceof(vm, obj, name_ptr, name_len) -> 0/1` — the
    /// SAME `RequiredPtr` helper (jit-api) the single-pass backend's 0xc1 arm
    /// calls; unlike `ldc_string_cp`/`ldc_class_cp`/`getstatic` this one is
    /// never absent, so `Op::InstanceOf`'s lowering does not need an
    /// OptionalPtr guard.
    pub(in crate::ir_lower) instanceof_check: usize,
    /// cov-05. `jit_checkcast(vm, obj, name_ptr, name_len) -> obj|0|i64::MIN`
    /// — same `RequiredPtr` status as `instanceof_check` above, the SAME
    /// helper the single-pass backend's 0xc0 arm calls.
    pub(in crate::ir_lower) checkcast: usize,
    /// cov-07. `jit_throw_exception(exc_ptr, bci) -> i64::MIN` (always) — the
    /// SAME `RequiredPtr` helper the single-pass backend's `0xbf` arm calls.
    /// Unlike `checkcast`/`instanceof` this takes no `vm_ptr`: it looks up the
    /// current JIT thread from TLS (`jit_thread_mut`), which `execute_jit_call`
    /// installs around every JIT call regardless of backend — so `Op::Throw`
    /// needs no `needs_context` plumbing of its own.
    pub(in crate::ir_lower) throw_exception: usize,
    /// `jit_set_throw_bci(bci)` — stamps THIS method's own throw-site bci onto
    /// `JitSignals::athrow_bci`, overwriting whatever a callee's compiled
    /// `athrow` lowering left there. Called from the shared exceptional-exit
    /// stub ([`Self::emit_call_exc_stub`]); see that function for why an exit
    /// without it silently drops a `finally`.
    ///
    /// `RequiredPtr` in `JitRuntimeHelpers`, and the SAME helper the
    /// single-pass backend's `emit_exception_check_stub` calls.
    pub(in crate::ir_lower) set_throw_bci: usize,
    /// Cooperative GC poll flag and no-argument slow path. IR values are
    /// canonicalized in frame slots, so the slow-path call needs no spill.
    pub(in crate::ir_lower) safepoint_flag_addr: usize,
    pub(in crate::ir_lower) safepoint_slow_path: usize,
    /// True iff the graph contains an `Op::Call` — then the method takes the VM
    /// context pointer as a hidden first argument (`try_call_with_context`), and
    /// the prologue stores it to `context_slot_off` + shifts the Java params.
    pub(in crate::ir_lower) needs_context: bool,
    /// Frame offset of the saved VM context pointer (valid iff `needs_context`).
    pub(in crate::ir_lower) context_slot_off: i32,
    /// Frame offset of the reserved safepoint-id slot (see the layout comment
    /// in `new`). Non-zero for every IR frame; the relocation contract needs it
    /// so the collector can tell which `OopMapEntry` describes this frame right
    /// now, rather than unioning every map in the method (unsound to relocate).
    pub(in crate::ir_lower) sp_id_slot_off: i32,
    pub(in crate::ir_lower) shadow_thread_slot_off: i32,
    /// Frame offsets (`[rbp - off]`) of every reference PARAMETER's prologue
    /// home. These are not node results, so the node scan cannot find them —
    /// see [`Lowerer::emit_safepoint_map`].
    pub(in crate::ir_lower) ref_param_homes: Vec<i16>,
    pub(in crate::ir_lower) shadow_savebase_slot_off: i32,
    /// Shadow `top` captured at method entry; restored by every exit. See the
    /// layout comment in `Lowerer::new`.
    pub(in crate::ir_lower) shadow_savetop_slot_off: i32,
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
    pub(in crate::ir_lower) phi_copy_scratch_slot_off: i32,
    /// `get_current_thread` helper; `0` when unwired (the JIT unit tests' stub
    /// table). Every shadow site is gated on it — dereferencing a thread
    /// pointer that was never fetched would read stack garbage.
    pub(in crate::ir_lower) get_current_thread: usize,
    /// Byte offset of the `ShadowStack` within `JvmThread`.
    pub(in crate::ir_lower) shadow_off_in_thread: i32,
    /// Round 13 wave 4 (lane mega3, M13-3): `JvmThread` displacement of the
    /// native-stack floor word, or 0 when it is not published / no thread
    /// mirror exists (`inline_cache_pic::ic_stack_floor_word_offset`).
    pub(in crate::ir_lower) stack_floor_off_in_thread: i32,
    /// Offsets pushed by the safepoint most recently emitted, consumed by the
    /// matching reload. Cleared on consumption so an unbalanced push cannot
    /// hand stale homes to a later reload.
    pub(in crate::ir_lower) pending_shadow: Vec<i16>,
    /// Byte span of the prologue's `get_current_thread` fetch, so it can be
    /// NOP-ed out when the method turns out never to publish.
    pub(in crate::ir_lower) thread_fetch_span: Option<(usize, usize)>,
    /// Byte span of every exit's shadow-`top` restore
    /// (`emit_shadow_savetop_restore`), erased with the fetch by
    /// `finish_lazy_thread_fetch` when the method never publishes: the restore
    /// then reads a thread slot that is always zero and always skips (round 11
    /// wave 11, lane `calls`).
    pub(in crate::ir_lower) savetop_restore_spans: Vec<(usize, usize)>,
    /// Did any safepoint actually emit a shadow push?
    pub(in crate::ir_lower) shadow_pushed_any: bool,
    /// The FRAME BLOCK: every frame word this method can ever publish as a
    /// reference — the reference-parameter homes plus the planned slot of
    /// every `Ref` node — sorted and unique. Non-empty only when the block is
    /// in use (`plan_frame_block`), in which case each safepoint publishes the
    /// whole block as indirect shadow entries the first time it runs in an
    /// activation, and never pushes or reloads values. See
    /// `gc::shadow_stack` (`Indirect entries`).
    pub(in crate::ir_lower) frame_block: Vec<i16>,
    /// Round 15 wave 1 (lane chain): the frame block's publication is ONE
    /// shared subroutine per body (`CRATONVM_JIT_IR_FRAME_BLOCK_SHARED_PUBLISH`),
    /// called from each safepoint's once-per-activation test. `true` once the
    /// plan chose that form for this body.
    pub(in crate::ir_lower) frame_block_shared: bool,
    /// When `true` around [`Self::emit_safepoint_map`], a `frame_block`
    /// safepoint records its metadata and emits its slot stores, while
    /// [`Self::emit_frame_block_ensure`] is deferred to the split ZGC
    /// allocation's cold stub arm.
    pub(in crate::ir_lower) defer_frame_block_ensure: bool,
    /// Position of the `rel32` of every `CALL` to the shared publication
    /// subroutine emitted before the subroutine itself; patched by
    /// [`Lowerer::emit_frame_block_publish_stub`].
    pub(in crate::ir_lower) frame_block_call_patches: Vec<usize>,
    /// Where the shared publication subroutine starts, once emitted. A site
    /// lowered after that calls it backwards directly.
    pub(in crate::ir_lower) frame_block_stub_at: Option<usize>,
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
    pub(in crate::ir_lower) compact_fields: HashMap<(usize, bool), (u32, bool, u8)>,
    /// Address of the GC's published `JIT_READ_BOUNDS` table, for the guarded
    /// receiver check. Zero ⇒ no inline field read (the guard cannot be
    /// emitted, so the helper stays).
    ///
    /// The READ table, not `JIT_REGION_BOUNDS`: this tier emits no inline
    /// reference STORE, so it asks only "is this address mapped, so a raw load
    /// cannot fault". The store question -- which G1/ZGC answer by leaving
    /// `JIT_REGION_BOUNDS` empty (`g1-audit.md` 8.1) -- has no site
    /// here to ask it.
    ///
    /// **Since 2026-09-02 there is such a site** — `emit_gated_ir_ref_putfield`
    /// below — and it asks the same READ question for the same reason: its
    /// barrier decision comes from the collector's published PLAN, not from a
    /// region table. `JIT_REGION_BOUNDS` is still never consulted here, so the
    /// G1/ZGC interlock that leaves it empty is untouched.
    pub(in crate::ir_lower) read_bounds_addr: usize,
    /// `(pre, post, young_floor)` — the collector's published reference-store
    /// barrier plan, resolved once at construction. `None` ⇒ no plan, and every
    /// reference store keeps the unconditional `jit_putfield_object` call.
    ///
    /// Read through the SAME `x64::objects` predicate the single-pass backend
    /// uses. Two tiers deciding independently what a published plan means is
    /// how one of them ends up skipping a barrier the other pays.
    pub(in crate::ir_lower) ref_store_gates: Option<(usize, usize, usize)>,
    /// zs s3/bt: the `pre_active | post_active << 8` word's address when the
    /// gated reference stores open with one fused test of both gates
    /// (`x64::zgc_fused_ref_store_gate_of`; ZGC's age-floor plan only).
    /// Resolved once at construction, like [`Self::ref_store_gates`].
    pub(in crate::ir_lower) zgc_fused_gate: Option<usize>,
    /// zs s5/bt2: the reference `Op::Store`s that store with no gate at all
    /// (`zgc_fresh_receiver_stores`; `CRATONVM_JIT_ZGC_FRESH_RECEIVER_STORE`,
    /// opt-in). Empty unless the switch is on AND [`Self::zgc_fused_gate`] is
    /// set (ZGC's age-floor plan).
    ///
    /// gcj j2/jt2: also built, by the same window rule, on the GENERATIONAL
    /// mask plan under `CRATONVM_JIT_IR_FRESH_STORE_NO_BARRIER` (default ON
    /// since 2026-10-25, `=0` kills; [`gen_fresh_store_no_pre_gate_for`]); there a member skips only the
    /// SATB pre gate (`emit_gated_ir_ref_putfield`). The two plans never meet
    /// in one compile (a floor plan has no mask, a mask plan no fused gate).
    pub(in crate::ir_lower) zgc_fresh_stores: std::collections::HashSet<NodeId>,
    /// `JIT_G1_BARRIER`'s address (`helpers.g1_barrier_addr`), for the G1
    /// inline reference store. `0` when not wired.
    pub(in crate::ir_lower) g1_barrier_addr: usize,
    /// JIT round 19 proposal wave 5, lane concat5 (STRGAP4-1): the header's
    /// class-id displacement (`helpers.class_id_offset_in_obj`) and the
    /// Generational backend's region-bounds table (`helpers.region_bounds_addr`),
    /// which the inline concat's String header and collector arm read.
    pub(in crate::ir_lower) concat_class_id_off: i32,
    pub(in crate::ir_lower) concat_region_bounds_addr: usize,
    /// g1put: `helpers.ref_store_pre_gate`, the SATB pre-gate byte, for the G1
    /// inline reference store (which publishes no plan, so
    /// [`Self::ref_store_gates`] is `None` there). `0` when not wired.
    pub(in crate::ir_lower) satb_pre_gate: usize,
    /// g1put: `helpers.g1_post_write_barrier`, the lean G1 remembered-set
    /// barrier the G1 inline store calls after a cross-region store. `0` when
    /// not wired.
    pub(in crate::ir_lower) g1_post_write_barrier: usize,
    /// gci i12/wb (`gci-i12wb-jit-barrier-review-findings-FIXED-20261015` item 3):
    /// `helpers.write_barrier`, the collector's card-only post barrier
    /// (`jit_write_barrier(vm_ptr, card_addr, value)`), which the inline card
    /// arms call when their POST test finds the card clean after the store.
    /// `0` when not wired: those arms then keep the full helper.
    pub(in crate::ir_lower) card_write_barrier: usize,
    /// The published post-barrier skip mask, when the plan uses that shape
    /// rather than the unsigned age floor.
    pub(in crate::ir_lower) ref_store_post_skip_mask: Option<u8>,
    /// The generational card view the gated `aastore` may check inline
    /// (`x64::gen_card_view_of`, the single-pass tier's own reading, resolved
    /// once): `(view address, element-precise marks)`. `None` without a valid
    /// view (G1, ZGC, a hand-built test table). Read only by
    /// `ir_inline_card_view` (round 13 wave 2, lane irhash, W7-2).
    pub(in crate::ir_lower) ir_card_view: Option<(usize, bool)>,
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
    pub(in crate::ir_lower) shadow_pushes: usize,
    pub(in crate::ir_lower) shadow_reloads: usize,
    /// Byte size of the Java-locals region, for the published `FrameLayout`.
    pub(in crate::ir_lower) locals_size: i32,
    /// First operand-spill offset, i.e. the exclusive top of the reserved
    /// locals region (locals + context + the five bookkeeping words: sp-id,
    /// cached thread, shadow save-base, shadow save-top, phi-copy scratch).
    pub(in crate::ir_lower) first_spill: i32,
    /// Frame offsets (`[rbp - off]`) of every slot the colouring proved holds
    /// no reference, computed once from [`SlotPlan::prim_colors`] and published
    /// on every safepoint map as `OopMapEntry::non_oop_stack_slots`. The IR
    /// tier's half of the stale-word oracle; nothing gates on it.
    pub(in crate::ir_lower) prim_slot_offsets: Vec<i16>,
    /// JIT round 25 (item 3, `CRATONVM_JIT_IR_PRIMITIVE_COLOUR_CLAIM`, default ON):
    /// the switch, read ONCE per compile in [`Lowerer::new`]. When set, this
    /// body publishes [`Self::prim_slot_offsets`] as its per-body claim
    /// (`CompiledMethod::ir_prim_colour_claim`, which the collector's band scan
    /// honours at every poll, the entry poll included) and the fresh-frame
    /// clear leaves those words out ([`Self::fresh_frame_clear_skip`]). One
    /// read for both halves, so the clear never leaves out a word the
    /// published claim does not name.
    pub(in crate::ir_lower) prim_colour_claim: bool,
    /// `CRATONVM_DBG_JITC` only: the bytes the prologue's fresh-frame clear
    /// would have written without the claim, and the bytes it writes with it
    /// (`(0, 0)` when no clear was emitted or the claim is off).
    pub(in crate::ir_lower) prim_colour_clear_bytes: (u32, u32),
    /// JIT round 25 wave 2 (`CRATONVM_JIT_IR_PINNED_PRIM_COLOUR_CLAIM`, default ON since j27,
    /// only together with [`Self::prim_colour_claim`]): the frame offsets of
    /// the `Pinned` colours [`claimable_pinned_nodes`] proved never hold a
    /// reference (a pinned colour has exactly one occupant, so its class is
    /// that node's type). They join the per-body claim and leave the clear
    /// ([`Self::body_claim_offsets_with`]); they are NOT added to the per-map
    /// `non_oop_stack_slots` list, which stays [`Self::prim_slot_offsets`].
    /// Empty unless both switches were on in [`Lowerer::new`].
    pub(in crate::ir_lower) pinned_prim_slot_offsets: Vec<i16>,
    /// JIT round 25 wave 2 (`CRATONVM_JIT_IR_DEOPT_IMAGE_CLAIM`, default ON since j27, only
    /// together with [`Self::prim_colour_claim`]): the words of the deopt GPR
    /// image ([`Self::deopt_gpr_image`]) join the per-body claim and leave the
    /// clear. Read once in [`Lowerer::new`]; the image geometry it names is
    /// final before the prologue (`size_save_bands` / `size_deopt_image` /
    /// `drop_unread_deopt_image` all run first), so the clear, the OSR entry
    /// stubs and the publication see one list.
    pub(in crate::ir_lower) deopt_image_claim: bool,
    /// JIT round 25 wave 3 (`CRATONVM_JIT_IR_HELPER_AREA_CLAIM`, default ON since j27, only
    /// together with [`Self::prim_colour_claim`], never on Win64): the 48-byte
    /// helper-argument area at the deep end of the frame (the 32-byte ABI
    /// shadow and the 16-byte stack-argument reserve) joins the per-body claim
    /// and leaves the clear. Read once in [`Lowerer::new`]; see
    /// [`ir_helper_area_claim_enabled`] for why no word of it is ever written
    /// on System V.
    pub(in crate::ir_lower) helper_area_claim: bool,
    /// `CRATONVM_DBG_JITC` only: the bytes the prologue's fresh-frame clear
    /// writes with no round-25 claim at all, and the bytes it writes (`(0, 0)`
    /// when no SSE-form clear was emitted). Printed by the `ir-frame-anatomy`
    /// line ([`Self::frame_anatomy`]).
    pub(in crate::ir_lower) frame_clear_bytes: (u32, u32),
    /// JIT round 25 wave 3 (`CRATONVM_JIT_IR_SNAPSHOT_RANGE_COLOURS`): the bci
    /// of the OSR entry stub being emitted, while `emit_osr_entry_stubs` emits
    /// one (cleared after it returns). A frame state built inside a stub may
    /// read a snapshot-ranged home only at a bci the plan recorded for it
    /// ([`Self::snapshot_ranged_home_ok`]).
    pub(in crate::ir_lower) snapshot_range_osr_bci: std::cell::Cell<Option<usize>>,
    /// Per-safepoint oop maps published for the moving-young relocation
    /// contract. An empty vector means "no precise coverage", which
    /// `conservative_roots` reads as a refusal — the fail-closed direction.
    pub(in crate::ir_lower) oop_maps: Vec<crate::OopMapEntry>,
    /// Nodes whose frame slot has been WRITTEN by already-emitted code. A
    /// `Ref` node's slot is uninitialised stack garbage until its defining node
    /// is emitted, so publishing it before then would hand the collector a
    /// bogus root out of the previous frame's leftovers.
    pub(in crate::ir_lower) defined_nodes: Vec<bool>,
    /// Monotonic safepoint id. Starts at 1: `sp_id_slot_off == 0` is the
    /// "no slot" sentinel on the reader side, and a zero id in the slot means
    /// "this frame has not reached a safepoint yet".
    pub(in crate::ir_lower) next_sp_id: u32,
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
    pub(in crate::ir_lower) sp_id_bcis: Vec<(u32, u32)>,
    /// JIT round 19 proposal wave 8, lane gc8 (GC719-1): `(safepoint id,
    /// frame words of the blocks held there, outermost first)` per safepoint
    /// that holds a `synchronized` block whose every lock has a reference home
    /// the map names ([`Self::safepoint_monitor_homes`]), in id order. Becomes
    /// `CompiledMethod::ir_monitor_homes`. Metadata only.
    pub(in crate::ir_lower) ir_monitor_homes: Vec<(u32, Box<[i32]>)>,
    /// `(return-address offset, method bci)` for every call site
    /// [`Self::note_inline_frame_return_site_at`] sees in a body that keeps no
    /// reference ([`Self::body_publishes_no_roots`]): the
    /// `CompiledMethod::return_site_bci_table` a `frame_holds_no_references`
    /// frame suspended in a direct JIT-to-JIT call is placed by (round 11 wave
    /// 14, lane fib; `r11w14-rt-ir-lower-oop-free-frame-patch`). Empty for every
    /// other body.
    pub(in crate::ir_lower) return_site_rows: Vec<(u32, u32)>,
    /// Round 11 wave 15 (lane fib; step 3 of
    /// `r11w14-rt-ir-lower-oop-free-frame-patch`): set by the `Op::Call` arm
    /// around its [`Self::emit_safepoint_map`] for a DIRECT self-call in a
    /// reference-free body, which then records the map and its id but emits
    /// no hot-path id store. See [`Self::deferred_self_call_sp_id`].
    pub(in crate::ir_lower) defer_next_sp_id_store: bool,
    /// The id that store would have written, handed to
    /// [`Self::emit_self_recursive_call`], which stores it on its two cold
    /// paths instead: before the stack-guard helper and at the head of the
    /// sentinel path (the deopt service and `dispatch_threw`). Those are the
    /// only places the frame is the INNERMOST compiled frame while the call is
    /// in progress, i.e. the only places the VM reads the raw slot
    /// (`conservative_roots::activation_bci`); below the innermost, a
    /// `frame_holds_no_references` frame is placed by its return address.
    pub(in crate::ir_lower) deferred_self_call_sp_id: Option<u32>,
    /// How many direct self-calls deferred their id store. The artifact fill
    /// refuses the compile when any did and the frame is not published as
    /// `frame_holds_no_references` after all: the collector would then trust
    /// a stale id.
    pub(in crate::ir_lower) self_call_id_stores_deferred: usize,
    /// Round 12 wave 8 (lane callcost): per node id, `true` for a direct
    /// self-call whose stack sample another direct self-call's sample has
    /// already answered on every path to it
    /// ([`Self::plan_self_call_sample_elision`]). Empty when nothing is
    /// elided.
    pub(in crate::ir_lower) self_call_sample_dominated: Vec<bool>,
    /// Set by the `Op::Call` arm for the one node it hands to
    /// [`Self::emit_self_recursive_call`], which takes it.
    pub(in crate::ir_lower) self_call_skip_sample: bool,
    /// JIT round 19 wave 4, lane throws4 (THROWS3-3,
    /// `CRATONVM_JIT_SELF_CALL_SENTINEL_LEAN`, opt-in, read once per compile).
    /// See [`Self::self_call_sentinel_lean_site`].
    pub(in crate::ir_lower) self_call_sentinel_lean_on: bool,
    /// Set by the `Op::Call` arm for the one node it hands to
    /// [`Self::emit_self_recursive_call`]: the call's result is narrow (void,
    /// `int`, reference), so it can never be the full-width `i64::MIN`.
    pub(in crate::ir_lower) self_call_result_narrow: bool,
    /// THROWS3-3: the next [`Self::emit_callee_deopt_service_cold`] ORs
    /// [`crate::SERVICE_ARGS_LEAN_PROPAGATE_BIT`] into its `num_args` word.
    /// Set and cleared around one call by the inline self-call route.
    pub(in crate::ir_lower) service_lean_bit_pending: bool,
    /// JIT round 19 wave 5, lane throws5 (THROWS4-1,
    /// `CRATONVM_JIT_DIRECT_CALL_SENTINEL_LEAN`, opt-in, read once per
    /// compile): the statically bound direct cross call and the cycle-edge
    /// cell call hand the callee-deopt service
    /// [`crate::SERVICE_ARGS_LEAN_PROPAGATE_BIT`] as well.
    pub(in crate::ir_lower) direct_call_sentinel_lean_on: bool,
    /// Set by the `Op::Call` arm for the one node it hands to
    /// [`Self::emit_direct_cross_call`]: the site is statically bound
    /// (`invokestatic` / `invokespecial`) and the switch above is on.
    pub(in crate::ir_lower) direct_call_lean_site: bool,
    /// How many direct self-calls the plan above covers, how many were lowered
    /// through [`Self::emit_self_recursive_call`], and how many of those
    /// emitted no sample. `lower_inner_attempt` refuses the compile when a
    /// sample was elided but not every planned site was lowered on that route
    /// (a dominator that emitted no sample would leave its dominated sites
    /// unchecked), or when the body carries an OSR entry (which enters past
    /// the dominators).
    pub(in crate::ir_lower) self_call_sites_planned: usize,
    pub(in crate::ir_lower) self_call_sites_lowered: usize,
    pub(in crate::ir_lower) self_call_samples_elided: usize,
    /// Round 12 wave 8 (lane callcost): the entry fast return's `Jcc rel32`
    /// patch site and what its outlined stub returns
    /// ([`Self::emit_entry_fast_return`]).
    pub(in crate::ir_lower) entry_fast_return: Option<(usize, EntryFastValue)>,
    /// Round 14 wave 5 (lane callcost, CC5-5): the entry fold's `JLE rel32`
    /// patch site and its table ([`Self::emit_entry_fold`]).
    pub(in crate::ir_lower) entry_fold: Option<(usize, EntryFold)>,
    /// Round 14 wave 6 (lane callcost2, CE-1): what the entry answers without
    /// a frame, for the direct self-calls ([`Self::emit_self_call_answer`]).
    pub(in crate::ir_lower) self_call_answer: Option<SelfCallAnswer>,
    /// Round 14 wave 7 (lane callcost3, CC6-2): the `(Op::Param node, GP
    /// register)` pairs the prologue filled straight from the incoming ABI
    /// register ([`Self::plan_param_register_fills`]).
    pub(in crate::ir_lower) param_register_fills: Vec<(NodeId, u8)>,
    /// Round 13 wave 8 (lane callcost3, CC2-1): the direct self-calls' cold
    /// halves, deferred past the body ([`Self::emit_deferred_self_call_tails`]).
    pub(in crate::ir_lower) deferred_self_call_tails: Vec<DeferredSelfCallTail>,
    /// zs s9/ca4 (`CRATONVM_JIT_ZGC_INLINE_ARM_FALL_THROUGH`): the cold halves
    /// of split inline ZGC allocations, deferred past the body
    /// ([`Self::emit_deferred_zgc_alloc_tails`]).
    pub(in crate::ir_lower) deferred_zgc_alloc_tails: Vec<DeferredZgcAllocTail>,
    /// JIT round 23 wave 9 (lane W9-L7, `CRATONVM_JIT_IR_GETFIELD_COLD_TAIL`):
    /// the legacy-layout read and the checked-helper call of inline field
    /// reads whose compact arm falls straight through to its join
    /// ([`Self::emit_deferred_getfield_tails`]).
    pub(in crate::ir_lower) deferred_getfield_tails: Vec<DeferredGetfieldTail>,
    pub(in crate::ir_lower) deferred_putfield_tails: Vec<DeferredPutfieldTail>,
    pub(in crate::ir_lower) deferred_gen_alloc_tails: Vec<DeferredGenAllocTail>,
    pub(in crate::ir_lower) deferred_checkcast_tails: Vec<DeferredCheckCastTail>,
    pub(in crate::ir_lower) preheader_epoch_guards: HashMap<usize, usize>,
    /// Round 14 wave 3 (lane calls, CC3-1): merged direct cross calls whose
    /// cold side went to a deferred tail, and those that kept it inline; and
    /// the inline-cache hit exits, with how many of them the CC2-1 admission
    /// (no roots, no pending shadow publication, an unprotected bci) would
    /// accept -- the census the IC half of CC3-1 is gated on. Printed under
    /// `CRATONVM_DBG_JITC=1`.
    pub(in crate::ir_lower) cross_call_cold_tails: (usize, usize),
    pub(in crate::ir_lower) ic_hit_exit_census: (usize, usize),
    /// Frame offset of `arg[0]` in the Java-argument staging region a call
    /// marshals its args into; `arg[i]` lives at `args_stage_top_off - i*8`
    /// (increasing address), and `args_ptr = rbp - args_stage_top_off`.
    pub(in crate::ir_lower) args_stage_top_off: i32,
    /// How many `arg[i]` words the staging region above holds (the method's
    /// widest call). gen r5w2/oomjit6: what `emit_dead_arg_stage_clears`
    /// zeroes.
    pub(in crate::ir_lower) args_stage_words: usize,
    /// Upper bound (inclusive) for a spill slot's frame offset — excludes the
    /// shadow space, the arg-staging region AND the callee-saved XMM save area,
    /// so spills never overlap any of them.
    pub(in crate::ir_lower) spill_cap_off: i32,
    /// Offset of `gpr[0]` in the deopt register image: the stub spills the
    /// whole file to `[rbp - deopt_regs_base .. +256)` and hands
    /// `ir_deopt_entry` a `*const SavedRegisters` pointing at it.
    ///
    /// Zero when [`ir_deopt_regs_bytes`] reserved nothing, which is the signal
    /// the stub reads to keep its historical two-argument shape.
    pub(in crate::ir_lower) deopt_regs_base: i32,
    /// Bytes the frame reserves for that image, from `[rbp - deopt_regs_base]`
    /// up: 0, the whole 256-byte `SavedRegisters`, or (round 16 wave 11, lane
    /// frames2, `CRATONVM_JIT_IR_DEOPT_IMAGE_SIZED`) only `gpr[0..=k]`, sized
    /// by [`Lowerer::size_deopt_image`] before the prologue is emitted. The
    /// stub stores exactly the words reserved, the published layout names
    /// exactly them, and no frame state may name a register outside them
    /// ([`Lowerer::deopt_image_holds_gpr`]).
    pub(in crate::ir_lower) deopt_regs_bytes: i32,
    /// Bytes reserved for the callee-saved XMM save area, `[spill_cap_off,
    /// spill_cap_off + this)`. See [`ir_saved_xmm_bytes`] and
    /// [`IR_LOWER_SAVED_XMMS`].
    ///
    /// Latched at construction rather than re-derived at emission time: the
    /// frame was *laid out* against this number, and a prologue that recomputed
    /// it from a flag read a second time would silently write outside its own
    /// reservation if the two reads ever disagreed.
    pub(in crate::ir_lower) saved_xmm_bytes: i32,
    /// The XMM registers the save band covers: [`IR_LOWER_SAVED_XMMS`], or
    /// (VEC21-7) [`IR_LOWER_SAVED_XMMS_VECTOR`] for a body whose vector
    /// pre-loops take XMM8-XMM11. Latched with `saved_xmm_bytes`.
    pub(in crate::ir_lower) saved_xmm_file: &'static [u8],
    /// Bytes reserved for the callee-saved GPR save area, immediately below
    /// the XMM one: `[spill_cap_off, spill_cap_off + saved_gpr_bytes)`.
    ///
    /// Latched at construction for the same reason `saved_xmm_bytes` is.
    pub(in crate::ir_lower) saved_gpr_bytes: i32,
    /// Round 16 wave 17 (lane frames2, `CRATONVM_JIT_IR_SAVE_BANDS_SIZED`):
    /// the two save bands were sized to the registers the residency plan hands
    /// out ([`Lowerer::size_save_bands`]), so the `k`-th saved register of a
    /// band sits in its `k`-th slot rather than in the slot of its index in
    /// the file. Set once, before the prologue; `saved_xmm_regs` /
    /// `saved_gpr_regs` read it, so the prologue, every epilogue, the OSR
    /// entry stubs and the published save list agree.
    pub(in crate::ir_lower) save_bands_packed: bool,
    /// Native offsets of `JE rel32` instructions emitted after each dispatch
    /// call (the exception sentinel check) that jump to the shared bail stub,
    /// each paired with the bytecode pc of the instruction whose exceptional
    /// exit it is.
    ///
    /// The bci is not decoration: the bail stub stamps it onto
    /// `JitSignals::athrow_bci` (`helpers.set_throw_bci`) so the interpreter's
    /// post-JIT routing range-tests THIS method's own exception table against
    /// THIS method's throw site. See [`Self::emit_call_exc_stub`].
    pub(in crate::ir_lower) call_exc_patches: Vec<(usize, usize)>,
    /// The method's protected ranges `[start_pc, end_pc)`, in method bcis —
    /// NON-EMPTY only on the precise-frame route (round 11 wave 10, lane
    /// irexc; `lib.rs` hands them over only under
    /// `CRATONVM_JIT_IR_PRECISE_HANDLER_FRAMES=1` for an RBC.6 method). A
    /// call-exception exit whose throw bci lies in one gets a reason-9 rethrow
    /// pad instead of a `call_exc_patches` row. See [`Self::push_rethrow_pad`].
    pub(in crate::ir_lower) protected_ranges: Vec<(u32, u32)>,
    /// The method bcis control can reach once one of its exception handlers
    /// has run (`deopt::pcs_reachable_from_handlers`, sorted); `None` =
    /// unknown, every bci counts as reached. Round 11 wave 11, lane irexc: an
    /// OSR entry at such a bci is withheld from a body with a rethrow pad
    /// ([`Self::osr_entry_closed_by_a_handler`]). Handed over with
    /// `protected_ranges`; empty off the precise-frame route, where no pad
    /// exists to consult it.
    pub(in crate::ir_lower) osr_handler_reachable: Option<Vec<u32>>,
    /// Rethrow pads not yet emitted, drained after the body by
    /// [`Self::emit_rethrow_pads`].
    pub(in crate::ir_lower) rethrow_pads: Vec<RethrowPad>,
    /// JIT round 19 wave 5 (lane handler5, SPLICE4-1): `jit_local_handler_lookup`
    /// (`JitRuntimeHelpers::local_handler_lookup`, `0` when unwired), the
    /// switch, read once, the armed call being lowered, the probes not yet
    /// emitted, and the sites they bake (moved onto the artifact's
    /// `_jit_local_handler_sites`, one per graph site index).
    pub(in crate::ir_lower) local_handler_lookup: usize,
    pub(in crate::ir_lower) catch_entry_on: bool,
    pub(in crate::ir_lower) cur_catch_arm: Option<CatchArmWire>,
    pub(in crate::ir_lower) catch_probes: Vec<CatchProbe>,
    pub(in crate::ir_lower) catch_site_boxes: Vec<Box<crate::JitLocalHandlerSite>>,
    /// `(first site, candidate count) -> catch_site_boxes index`
    /// (HANDLER19-3 keys by the ladder's cut too; see `catch_site_box`).
    pub(in crate::ir_lower) catch_site_index: HashMap<(u16, usize), usize>,
    /// By-value copies of the reason-9 points the pads publish, appended to
    /// the artifact's `deopt_points` so the by-bci consumers find them — the
    /// OSR exception transfer asks `osr_exit::exceptional_reason_at_bci` of
    /// that list, exactly as it does of a single-pass artifact's.
    pub(in crate::ir_lower) rethrow_points: Vec<DeoptimizationPoint>,
    /// A call-exception exit that publishes NOTHING (outside every protected
    /// range) was emitted at a bci whose snapshot holds a monitor, or whose
    /// snapshot could not be found. Such an exit cannot be propagated out of
    /// an OSR'd frame without leaking the lock, so it withdraws the body's
    /// `ir_osr_exception_exit_bcis` fact.
    pub(in crate::ir_lower) propagating_exit_holds_monitor: bool,
    /// Round 15 wave 1 (lane sync, SS8-2): the method monitor of a
    /// SELF-LOCKING body (`Graph::method_self_lock`), or `None`. While set,
    /// [`Self::emit_call_exc_stub`] releases it on every exceptional exit
    /// (JVMS 2.11.10: an admitted method has no exception table, so each one
    /// leaves the method), the deopt exits keep it (their frames hand it to
    /// the resumed frame), the body gets no entry fast return and no OSR
    /// entry, and [`Self::self_lock_refusal`] refuses any other shape.
    pub(in crate::ir_lower) self_lock: Option<crate::ir::MethodSelfLock>,
    /// Round 15 wave 1 (lane sync, SS8-2): the exceptional edges of the method
    /// monitor's OWN enter and exits, `(patch, bci)`, which leave WITHOUT a
    /// release (the enter took nothing; a failed exit must not be retried and
    /// have its exception replaced). Emitted by [`Self::emit_call_exc_stub`].
    pub(in crate::ir_lower) self_lock_unheld_exc_patches: Vec<(usize, usize)>,
    /// Guard boxes classified, as they were emitted, EXACT or VALUE-EQUIVALENT
    /// (tier proposal 27 phase 3, round 11 wave 12, lane irexc): counted in
    /// [`Self::emit_deopt_unless`] for an exact reason and in
    /// [`Self::emit_value_equivalent_deopt_unless`], and (wave 13) in the
    /// `Op::Guard` arm for the shapes [`Self::op_guard_is_exact`] recognises.
    /// `osr_exit::guard_exit_fact` compares it with the guard boxes actually
    /// pushed, so a box nobody classified (every other `Op::Guard`) withdraws
    /// `ir_osr_guard_exit_bcis`.
    pub(in crate::ir_lower) classified_guard_exits: usize,
    /// Round 16 (lane matrix, MX16-1): where `emit_deopt_stub` put the shared
    /// deopt stub, `None` before it ran or when the body has no guard. An OSR
    /// entry stub's re-check failure (`emit_osr_entry_rechecks`) jumps there.
    pub(in crate::ir_lower) deopt_stub_off: Option<usize>,
    /// Whether, and on which verdict bit, an unconditional back-edge poll
    /// leaves the body at the loop header when its slow path says so
    /// ([`Self::back_edge_mode_exit_state`]): the optimizing OSR door's
    /// compile (`CompileRequest::osr_mode_exit_polls`; interpreter round i1
    /// wave 15, lane L2) or, since wave 18 (lane L2), a method-entry compile
    /// whose exit every resume sink takes ([`ir_entry_mode_exits_admitted`]).
    /// [`IrPollModeExits::Off`] emits exactly what it did before wave 15.
    pub(in crate::ir_lower) poll_mode_exits: IrPollModeExits,
    /// The optimizer relied on a trap-free body and dropped values a
    /// safepoint snapshot named (`ir_optimize::take_unroll_used_unreachable_frames`:
    /// a strength-reduced multiply's snapshot slots become `NO_NODE`). Shape 2
    /// (b) of the i41 page: the back-edge exits are kept, and an exit whose
    /// header state reads `NO_NODE` in a LIVE slot refuses
    /// ([`Self::state_lost_a_value_to_the_optimizer`]); before, every poll
    /// exit of the compile was switched off.
    pub(in crate::ir_lower) optimizer_dropped_snapshot_values: bool,
    /// This compile's mode-exit census, laid out like a row of
    /// `IR_POLL_MODE_EXIT_CENSUS` (slot 0: polls given an exit; slot `1 + i`:
    /// refused for `ModeExitRefusal::CAUSE_NAMES[i]`); all zero when no exit
    /// is sought ([`Self::note_mode_exit_outcome`], wave 17).
    pub(in crate::ir_lower) mode_exit_census: [usize; 9],
    /// The `OsrExit` points those exits publish. Kept apart from
    /// `deopt_boxes` until the OSR facts (`osr_sentinel_free`, the guard and
    /// exception exit lists) have been read, so an exit that resumes at a
    /// state the graph itself reaches changes no admission; appended to the
    /// artifact's boxes afterwards.
    pub(in crate::ir_lower) poll_exit_boxes: Vec<Box<DeoptimizationPoint>>,
    /// `(jcc_patch, point, cell write-backs)` of each poll exit, emitted by
    /// [`Self::emit_poll_exit_pads`] after those facts, like `deopt_pads`.
    /// The write-backs (JIT round 19 wave 4, lane osr4, OSR2-1) are empty
    /// except at a promoted loop header ([`PollModeExit`]).
    pub(in crate::ir_lower) poll_exit_pads: Vec<(usize, u64, Vec<CellWriteBack>)>,
    /// JIT round 19 wave 5 (lane osr5, OSR2-3,
    /// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_DEOPT`): the promoted loops'
    /// pending-store plan ([`plan_promoted_pending`]), computed on first need.
    /// `Ok(None)` for every graph without kept (silent) cell stores.
    pub(in crate::ir_lower) promoted_pending:
        std::cell::OnceCell<Result<Option<PromotedPending>, &'static str>>,
    /// OSR2-3: the write-backs every deopt pad the node being lowered emits
    /// must run first (set across one node by [`Self::begin_promoted_pending`]).
    /// `None` outside a promoted loop with kept stores, and inside one when a
    /// cell's value is not describable here (any deopt then refuses the
    /// compile).
    pub(in crate::ir_lower) pad_writebacks: Option<Vec<CellWriteBack>>,
    /// OSR2-3: how many natural pads took `pad_writebacks` in this node.
    pub(in crate::ir_lower) pad_writebacks_attached: usize,
    /// OSR2-3: the write-backs of a deopt pad, by the pad's `Jcc` patch
    /// offset; emitted by [`Self::emit_deopt_pads`] before the point load.
    pub(in crate::ir_lower) deopt_pad_writebacks: HashMap<usize, Vec<CellWriteBack>>,
    /// OSR2-3: the points whose natural pad took write-backs, for the
    /// whole-body coverage check ([`Self::check_promoted_pending_coverage`]).
    pub(in crate::ir_lower) covered_pad_points: std::collections::HashSet<u64>,
    /// JIT round 19 wave 7 (lane osr7, OSR6-1,
    /// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_RESIDENT`): the residency
    /// planner's live range `[lo, hi]` of every value, in its own position
    /// numbering, and that numbering ([`Self::pending_ls_pos`]). Filled only
    /// under the switch, for a graph with silent cell stores and an installed
    /// register plan; empty otherwise, and then no pad describes a value by
    /// residency ([`Self::pending_resident_reg`]).
    pub(in crate::ir_lower) pending_ls_range: Vec<Option<(usize, usize)>>,
    /// The position of each node in [`Self::pending_ls_range`]'s numbering.
    pub(in crate::ir_lower) pending_ls_pos: Vec<Option<usize>>,
    /// The method's own bytecode, when the door handed it over, for the
    /// bytecode liveness [`Self::poll_exit_local_dead`] asks (interpreter
    /// round i1 wave 18, lane L2). `None`: no local is ever called dead.
    pub(in crate::ir_lower) poll_exit_bytecode: Option<PollExitBytecode<'a>>,
    /// `regalloc::live_locals_per_pc_all` over `poll_exit_bytecode`, computed
    /// the first time a mode exit asks, so a compile whose header states are
    /// all describable pays nothing.
    pub(in crate::ir_lower) poll_exit_liveness:
        std::cell::OnceCell<Option<(Vec<u64>, Vec<bool>, usize)>>,
    /// Safepoint polls whose slow path was NOT emitted where the poll is, one
    /// entry per poll site: `(jcc_patch, resume, spill_high_water)`.
    ///
    /// `jcc_patch` is the rel32 field of the site's `JNZ`, patched to the
    /// outlined block when [`Self::emit_outlined_polls`] emits it; `resume` is
    /// the byte after that `JNZ`, which the outlined block jumps back to; and
    /// `spill_high_water` is one of the per-site inputs to the slow path's
    /// content.
    ///
    /// Round 9: it is NOT the only one, as this comment used to claim.
    /// `emit_safepoint_map` also reads `defined_nodes` (which `Ref` slots to
    /// publish) and `cur_bci` (the id->bci row), and by the time the outlined
    /// block is emitted — after the whole body — both describe the END of the
    /// method: a loop's back-edge poll published the slots of `Ref` values
    /// defined after the loop (never written on this path, so previous-frame
    /// garbage handed to the collector as roots) and mapped its id to the last
    /// lowered bci. The next two elements snapshot both at the poll site; the
    /// sixth is the site's mode exit (wave 15, see `poll_mode_exits`).
    ///
    /// Round 16 (lane codegen): the seventh is `cur_node_pc` at the site. The
    /// map's inline-frame row (`inline_frame_sp_id_rows`, round 14 wave 2)
    /// is keyed by it, and after the body it names the LAST lowered node: an
    /// outlined method-entry poll of a body whose last node sat in a spliced
    /// callee published that callee's inline chain for the entry safepoint, so
    /// a stack walk of a thread stopped there showed phantom inlined frames.
    ///
    /// Round 17 (CG16P-1): the eighth is the back-edge block of an outlined
    /// BACK-EDGE poll (`None` at the method entry), whose FP reloads the
    /// outlined block emits after the call exactly as the inline arm does
    /// ([`ir_back_edge_poll_outline_enabled`]).
    ///
    /// The ninth is the dead-home words a back-edge poll zeroes before its
    /// map ([`ir_poll_slow_path_dead_home_clear_enabled`]; empty otherwise).
    /// The last entry holds the phi source registers and their authoritative
    /// homes, captured at the same poll. The final entry snapshots the GP
    /// register owners at the poll, before later code can reuse them.
    pub(in crate::ir_lower) outlined_polls: Vec<(
        usize,
        usize,
        i32,
        Vec<bool>,
        usize,
        Option<PollModeExit>,
        Option<usize>,
        Option<usize>,
        Vec<i32>,
        Vec<(u8, i16)>,
        [Option<NodeId>; 16],
    )>,
    /// Round 16 (lane codegen, CG16-2): the integer divisions' rare fixups
    /// (the zero-divisor placeholder and the `-1` divisor's answer) moved
    /// after the body, one entry per fixup; emitted by
    /// [`Self::emit_div_cold_fixups`]. Empty unless [`ir_div_cold_fixups_enabled`].
    pub(in crate::ir_lower) div_cold_fixups: Vec<DivColdFixup>,
    /// Buffer offsets of the condition-code byte of every poll whose slow
    /// path may leave this body (a poll with a mode exit): the inline `JZ`'s
    /// `0x84`, the outlined `JNZ`'s `0x85`. Published as
    /// `CompiledMethod::exit_poll_sites` so a class redefinition that
    /// withdraws the body can force them (`JitCache::force_withdrawn_exit_polls`;
    /// interpreter round i1 wave 25, lane L6).
    pub(in crate::ir_lower) exit_poll_sites: Vec<u32>,
    /// Post-call exit sites the walk emitted whose stubs
    /// [`Self::emit_post_call_exit_stubs`] has yet to lay down, as `(site
    /// offset, point)` (interpreter round i1 wave 28, lane L6;
    /// [`Self::maybe_emit_post_call_exit_site`]).
    pub(in crate::ir_lower) post_call_exits_pending: Vec<(usize, u64)>,
    /// `(site, stub)` of every post-call exit site, published as
    /// `CompiledMethod::post_call_exit_sites` so a class redefinition that
    /// withdraws the body forces them with its exit polls (wave 28, lane L6).
    pub(in crate::ir_lower) post_call_exit_sites: Vec<(u32, u32)>,
    /// fib44-fix follow-up: native offsets of the rel32 operand of each direct
    /// self-recursive `CALL` (invoke_kind 4), patched at finalize to target the
    /// method's own entry (code offset 0). See `lower_self_call` / Op::Call.
    pub(in crate::ir_lower) self_call_patches: Vec<usize>,
    /// IR direct-call lowering: statically-bound call sites this compile may
    /// lower as a raw `CALL` into an already-compiled callee's entry instead of
    /// routing through the generic `jit_invoke_dispatch` helper. Keyed by the
    /// invoke's bytecode pc (the same key the IR builder stamps on the
    /// `Op::Call` node via `Node::bytecode_pc`); the value is
    /// `(callee_entry, callee_needs_context)` as returned by the caller's
    /// `callee_compiler`. Empty (the default, and whenever the direct-call gate
    /// is off) ⇒ every `Op::Call` keeps the historical helper dispatch,
    /// byte-for-byte.
    pub(in crate::ir_lower) direct_calls: &'a HashMap<usize, (usize, bool)>,
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
    pub(in crate::ir_lower) ic_slots: &'a HashMap<usize, (usize, usize)>,
    /// Round 9 wave 8 (irl8): the IR tier's compile-time recursion-cycle
    /// CLOSING edges, `pc -> JitCycleEdgeCell address`
    /// (`perf-mutual-recursion-forces-every-call-through-invoke-dispatch`).
    /// Keyed by the invoke's bytecode pc, like [`Self::direct_calls`]. A site
    /// here is lowered by [`Self::emit_cycle_edge_cell_call`]: a guarded
    /// indirect call through the cell's entry word, and the unchanged
    /// `jit_invoke_dispatch` while the word is `0`. Filled only through
    /// [`lower_inner_with_cycle_cells`]; empty (every other door) keeps the
    /// historical dispatch, byte-for-byte. The cells are owned by the
    /// artifact the caller attaches them to (`CompiledMethod::_jit_cycle_cells`),
    /// exactly as the single-pass tier's are.
    pub(in crate::ir_lower) cycle_cells: HashMap<usize, usize>,
    /// Round 9 wave 8b (irl8 request 3): class ids whose inline-TLAB `new`
    /// calls `helpers.zgc_note_tlab_object` instead of `tlab_post_init`
    /// (post-init no-op classes, ZGC announce mode). Filled only through
    /// [`lower_inner_with_cycle_cells`], and only when `lib.rs` planned it
    /// (`CRATONVM_JIT_IR_ZGC_ANNOUNCE`); empty keeps `tlab_post_init`.
    pub(in crate::ir_lower) tlab_announce_classes: std::collections::HashSet<u32>,
    /// Round 9 wave 9 (irl9): THIS compile's `java/util/ArrayList` layout
    /// (`StringFieldLayout::array_list`), for the optimizing tier's inline
    /// `ArrayList.get(int)` / `size()` prefix
    /// ([`Self::try_emit_ir_arraylist_access_prefix`]). Filled only through
    /// [`lower_inner_with_array_list`]; `None` (every other door) keeps every
    /// call site's lowering byte-for-byte. Per compile, never a process-wide latch
    /// (`ir::PUBLISHED_STRING_LAYOUT` seeds no builder since round 11 wave 3):
    /// the class id and compact offsets are per VM.
    pub(in crate::ir_lower) array_list_layout: Option<crate::ArrayListFieldLayout>,
    /// nio20 wave 3, lane L2: THIS compile's `java/lang/String` layout, for the
    /// optimizing tier's inline String search prefix
    /// ([`Self::try_emit_ir_string_search_prefix`]). Read from
    /// [`CompileStringLayoutScope`] by `lower_inner_attempt`; `None` (every
    /// door but `lib.rs`'s IR door, every unit test) keeps every call site's
    /// lowering byte-for-byte. Per compile, never a process-wide latch.
    pub(in crate::ir_lower) string_layout: Option<crate::StringFieldLayout>,
    /// The hit edge's `JMP rel32` of an inline call prefix emitted by the
    /// node being lowered, patched by `lower_data_node_tracked` to the byte
    /// after that node's whole lowering (the call arm has many early
    /// `return`s, so the join cannot be placed inside it).
    pub(in crate::ir_lower) call_prefix_join: Option<usize>,
    /// `helpers.zgc_note_tlab_object`; `0` when not wired (then no site
    /// announces).
    pub(in crate::ir_lower) zgc_note_tlab_object: usize,
    /// Address of the `jit_invoke_virtual_mic` runtime helper — the
    /// inline-cache miss path, which resolves the receiver AND populates both
    /// caches. 6 args: `(vm_ptr, info_ptr, args_ptr, num_args, mic, pic)`.
    /// 0 ⇒ no IC site may be planned (the planner checks the same field).
    pub(in crate::ir_lower) invoke_virtual_mic: usize,
    /// Shared post-call frame publication used by direct and hashed dispatch
    /// stubs. Zero when precise frame tracking is unavailable.
    pub(in crate::ir_lower) frame_record: usize,
    /// Segment-relative displacement of the compile-id mirror, and this
    /// compilation's identity — the pair that lets the GC name the method
    /// owning the innermost RBP without decoding the call that created the
    /// frame. Both 0 when unavailable → nothing is published.
    pub(in crate::ir_lower) inline_cm_tls_disp: usize,
    pub(in crate::ir_lower) compile_id: crate::CompileIdReservation,
    /// `jit_service_callee_deopt` — services a compiled callee's `i64::MIN`
    /// deopt sentinel at the megamorphic stub's inline call site, so the
    /// callee's stashed frame is resumed there instead of escaping to the
    /// caller as if the caller had deopted. Zero disables the emitted check.
    /// See `runtime_lowering::emit_callee_deopt_check`.
    pub(in crate::ir_lower) service_callee_deopt: usize,
    /// wire-tiered-manager Step 4 (PGO handoff C1 → C2): per-bytecode-PC branch
    /// bias, keyed by the conditional-branch instruction's bytecode PC (the same
    /// key the IR builder stamps on each `Op::If` via `Node::bytecode_pc`). Value
    /// `true` = the branch is usually TAKEN, `false` = usually NOT taken; an
    /// absent PC is inconclusive. Only `Some(false)` (usually-not-taken) changes
    /// codegen — see `lower_terminator`'s `Op::If` arm. Empty (the default, and
    /// whenever profiling is off) ⇒ every `Op::If` keeps its historical layout
    /// byte-for-byte.
    pub(in crate::ir_lower) branch_hints: &'a HashMap<usize, bool>,
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
    pub(in crate::ir_lower) spliced_ranges: &'a [(usize, usize, usize)],
    /// Guard-surviving scalar replacement: metadata for each scalar-replaced
    /// `Op::New` so a deopt snapshot slot holding it lowers to a
    /// `FrameValue::VirtualObject`. `None` ⇒ disabled (byte-identical default).
    pub(in crate::ir_lower) sr_map: Option<&'a ScalarReplacementMap>,
    /// Bytecode indices with a safepoint snapshot in `graph.safepoints`.
    ///
    /// This and the next two are indexes over `graph`, gathered once in
    /// [`Lowerer::new`] — the lowerer holds the graph by shared reference, so
    /// they cannot go stale. Each replaces a scan that ran once per division
    /// ([`Lowerer::emit_div_zero_guard`]) or once per snapshot
    /// ([`Lowerer::deopt_block_for_snapshot`]), which a method near the node cap
    /// paid as nodes × divisions.
    pub(in crate::ir_lower) safepoint_bcis: std::collections::HashSet<usize>,
    /// For each bci, the index of the FIRST `graph.safepoints` entry at that
    /// bci. That is the entry the by-bci scans
    /// (`safepoints.iter().position(|s| s.bci == bci)`) found. Read by
    /// [`Lowerer::resolve_frame_state_for_bci`], which ran that scan once per
    /// deopt site, and by the per-block OSR entry planner, which ran it once
    /// per block. See [`Lowerer::first_safepoint_index_by_bci`].
    ///
    /// Frames of the compiling method only, like [`Self::safepoint_bcis`]: a
    /// chain snapshot recorded inside a spliced body (round 13 wave 3,
    /// `ir::SpliceScopeChain`) is indexed in
    /// [`Self::splice_state_first_by_pc`] instead, so every by-bci consumer
    /// answers exactly as it did before those snapshots existed.
    pub(in crate::ir_lower) safepoint_first_by_bci: HashMap<usize, usize>,
    /// Round 13 wave 3 (lane framestate, W5-3 stage 2): for each combined pc
    /// inside a spliced body, the index of the first CHAIN snapshot there
    /// (`ir::SafepointSnapshot::is_splice_state`). Empty unless the build ran
    /// under `CRATONVM_JIT_IR_SPLICE_FRAME_STATES`. Read only by
    /// [`Lowerer::splice_chain_for_site`].
    pub(in crate::ir_lower) splice_state_first_by_pc: HashMap<usize, usize>,
    /// Interpreter round i1 wave 47, lane L3: each spliced call's post-call
    /// chain snapshot ([`Lowerer::post_call_splice_states_by_call`]). Empty
    /// unless the build ran under `CRATONVM_JIT_IR_POST_CALL_SPLICE_EXITS`.
    pub(in crate::ir_lower) post_call_splice_state_by_call: HashMap<NodeId, usize>,
    /// Post-call exit sites whose frame is a caller chain (a call inside a
    /// splice), for the `[ir] post-call-chain-sites` line.
    pub(in crate::ir_lower) post_call_chain_sites: usize,
    /// Back-edge polls of a loop inside a splice given a caller-chain mode
    /// exit (interpreter round i2 wave 57, lane L2;
    /// [`Lowerer::spliced_back_edge_mode_exit_state`]): published as
    /// `CompiledMethod::chain_poll_exits`.
    pub(in crate::ir_lower) spliced_header_poll_exits: usize,
    /// Back-edge poll exits whose state names a scalar-replaced object as a
    /// `VirtualObject` (`CRATONVM_JIT_IR_BACK_EDGE_VIRTUAL_OBJECT_EXITS`), for
    /// the `ir-exit-cost-census` line only (nio21 wave 4 L3).
    pub(in crate::ir_lower) poll_exits_naming_virtuals: usize,
    /// Guard points in spliced bodies published with a caller chain, and
    /// those that fell back to the flat frame at the outermost `invoke` (the
    /// chain could not be made VM-resumable). `[ir] splice-frame-states` line
    /// under `CRATONVM_DBG_IR_COMPILES=1` (or `CRATONVM_DBG_JITC=1`).
    pub(in crate::ir_lower) splice_chain_points: usize,
    pub(in crate::ir_lower) splice_chain_fallbacks: usize,
    /// Round 15 wave 1 (lane chain, hand-back): the nodes scheduled inside a
    /// synchronized splice's monitor window (a pairing
    /// [`Self::sync_splice_region_refusal`] accepts), computed on first use.
    /// A deopt planted while lowering one of them publishes no caller chain:
    /// see [`Self::sync_window_site_publishes_no_chain`].
    pub(in crate::ir_lower) sync_window_nodes:
        std::cell::OnceCell<std::collections::HashSet<NodeId>>,
    /// How many deopt sites that exemption kept on the flat frame (the
    /// `[ir] splice-frame-states` line).
    pub(in crate::ir_lower) sync_window_flat_sites: usize,
    /// Round 15 wave 6 (lane sweepA, `r15w4-chain5-review-findings-FIXED-20260930.md`):
    /// the live nodes [`Self::chain_site_fires_at`] can match at each raw pc
    /// (an `Op::Guard` by its anchor `bci`, any other node by its
    /// `bytecode_pc`), in id order, built on the first
    /// [`Self::chain_deopt_block_and_span`] call, so a body whose chain
    /// snapshots name scalar-replaced objects no longer scans every node once
    /// per chain point.
    pub(in crate::ir_lower) chain_site_candidates_by_pc:
        std::cell::OnceCell<HashMap<usize, Vec<NodeId>>>,
    /// Round 13 wave 11 (lane chain5, CH-1): `CRATONVM_DEOPT_EAGER_CHAINS`,
    /// read once per lowering ([`crate::ir::ir_forced_chain_deopts_enabled`]).
    pub(in crate::ir_lower) forced_chain_deopts: bool,
    /// The arrival counters the forced chain deopts bump
    /// ([`Lowerer::emit_forced_chain_deopt`]), one per chain point. The code
    /// bakes their addresses, so they move into the artifact
    /// (`CompiledMethod::_forced_chain_deopt_counters`) with the deopt boxes.
    /// ([`crate::deopt::ForcedChainTripCell`]).
    pub(in crate::ir_lower) forced_chain_deopt_counters:
        Vec<Box<crate::deopt::ForcedChainTripCell>>,
    /// The point each cell above belongs to, index for index
    /// (`CompiledMethod::_forced_chain_deopt_points`).
    pub(in crate::ir_lower) forced_chain_deopt_points: Vec<u64>,
    /// JIT round 21 lane stress (FUZZ21-1 / FUZZ21-7): the forced-arrival
    /// period of every back-edge poll, `0` off; read once per lowering
    /// ([`dbg_poll_stress`]). See [`Lowerer::emit_poll_stress_countdown`].
    pub(in crate::ir_lower) poll_stress_period: u32,
    /// The `:seed` of the same switches, staggering the sites' first forced
    /// arrival ([`poll_stress_initial_countdown`]).
    pub(in crate::ir_lower) poll_stress_seed: Option<u32>,
    /// The per-poll countdown cells the forced arrivals decrement. The code
    /// bakes their addresses, so they move into the artifact
    /// (`CompiledMethod::_poll_stress_cells`) like the chain cells above.
    pub(in crate::ir_lower) poll_stress_cells: Vec<Box<std::sync::atomic::AtomicU32>>,
    /// JIT round 21 lane stress (FUZZ21-3): `CRATONVM_DBG_DEAD_HOME_POISON`,
    /// read once per lowering: the dead-home clears write
    /// [`cratonvm_types::jit_activation::DEAD_HOME_POISON`] instead of `0`
    /// ([`Lowerer::emit_dead_home_clear`]).
    pub(in crate::ir_lower) dead_home_poison: bool,
    /// Bytecode indices an `Op::Guard { bci }` is anchored at — RAW pcs, not
    /// resume bcis, deliberately. The one question asked of this set is "is a
    /// guard anchored beside THIS division" (`emit_div_zero_guard`,
    /// [`Lowerer::fires_deopt_at`]), and a division pairs with the guard the
    /// builder planted at its own pc. Mapped through `resume_bci`, every
    /// division inside a splice that contains ANY guard would read as
    /// anchored, and the unanchored ones would lose their zero test.
    pub(in crate::ir_lower) guard_bcis: std::collections::HashSet<usize>,
    /// Every node a deopt at a bci can fire from, in node-id order, keyed by
    /// the bci the deopt RESUMES at (`resume_bci` of the node's pc): each
    /// `Op::Guard { bci }`, each `Op::Div` / `Op::Rem`, and every other node
    /// that can transfer to the interpreter (`!op_cannot_deopt`).
    ///
    /// Round 9 wave 2: guards and divisions were keyed by their RAW pc while
    /// every other site (and every emitter) used the resume bci, so a guard or
    /// division inside a spliced body was invisible to `deopt_block_for_snapshot`
    /// at the enclosing invoke — see [`Lowerer::index_deopt_sites`].
    ///
    /// Before 2026-09-12 only guards and divisions were indexed, so a
    /// scalar-replaced object live at a call, a field access or an allocation
    /// had no deopt block and could never be described.
    pub(in crate::ir_lower) deopt_sites_by_bci: HashMap<usize, Vec<NodeId>>,
    /// The reference every LIVE `Op::MonitorEnter` / `Op::MonitorExit` names.
    ///
    /// A snapshot monitor entry naming one of these is a lock the compiled
    /// code really took (through the monitor helper or its inline thin-lock
    /// fast path, which records the lock in the same two places), so the thread still holds
    /// it when the frame deoptimizes and the resume must NOT take it again
    /// (`MonitorInfo::relock == false`). An entry naming anything else had its
    /// monitor ops elided by lock elision — they are `Op::Dead` with their
    /// inputs cleared, which is exactly why they cannot appear here — so the
    /// compiled code never locked it and the resume must
    /// (`MonitorInfo::relock == true`). Escape analysis' elision is
    /// all-or-nothing per object, so one live op on an object means none of
    /// its monitors were elided BY IT. The one partial elision, nested-lock
    /// elision, marks the entries it removed per snapshot
    /// (`SafepointSnapshot::monitor_elided_mask`; see `resolve_monitors`).
    pub(in crate::ir_lower) live_monitor_operands: std::collections::HashSet<NodeId>,
    /// Value phis grouped by their merge control (`phi.inputs[0]`), each list
    /// in ascending node-id order. Built once in `new` so that
    /// `gather_phi_copies` — asked up to three times per conditional edge
    /// (`edge_has_phi_copies` on both successors, then `emit_phi_copies`) — no
    /// longer rescans every node of the graph per edge, which made phi-copy
    /// gathering `O(nodes * edges)` on large methods. Same filter and the same
    /// order as the scan it replaces, so the copies it yields are identical.
    pub(in crate::ir_lower) phis_by_merge: HashMap<NodeId, Vec<NodeId>>,
    /// Combined-buffer pc → the spliced callees enclosing it, innermost first.
    ///
    /// Empty unless `lib.rs` spliced a body into this compile. Non-empty, it is
    /// what lets a stack trace through an optimizing-tier frame show the
    /// callees this tier inlined — the gap `conservative_roots`'s
    /// `compiled_frame_inline_chain` documented as "IR-tier inlining therefore
    /// still contributes no frames". See [`ir::IrInlineFrameSites`].
    pub(in crate::ir_lower) inline_frame_sites: &'a crate::ir::IrInlineFrameSites,
    /// Rows accumulated at call return sites inside spliced bodies, becoming
    /// `CompiledMethod::inline_frame_map`. Empty ⇒ the artifact carries the
    /// same empty map it always did.
    pub(in crate::ir_lower) inline_frame_rows: Vec<crate::x64::InlineFrameRow>,
    /// The inline-frame-row census of this compile, open only under
    /// `CRATONVM_DBG_INLINE_ROW_CENSUS=1` for a compile that spliced something
    /// ([`Self::begin_inline_row_census`]; interpreter round i1 wave 49, lane
    /// L3). `None` otherwise: one `is_some` test per lowered node.
    pub(in crate::ir_lower) row_census: Option<Box<InlineRowCensus>>,
    /// Round 14 wave 2 (lane trace): `(safepoint id, chain at that point)` for
    /// every id [`Self::emit_safepoint_map`] issued while this compile had
    /// spliced bodies -- the innermost frame's exact key
    /// (`InlineFrameMap::chain_for_safepoint_id`). Empty chains are kept until
    /// the finalize so a rewound id cannot keep a stale chain.
    pub(in crate::ir_lower) inline_frame_sp_id_rows: Vec<(u32, Vec<crate::x64::InlineFrameLevel>)>,
    /// Interpreter round i2 wave 58 (lane L2,
    /// `CRATONVM_JIT_IR_SYNC_SPLICE_ENTER_NAMES_CALLEE`): the combined pc the
    /// NEXT [`Self::emit_safepoint_map`] takes its inline chain from instead of
    /// `cur_node_pc` -- the spliced synchronized callee's first instruction,
    /// set by a synchronized splice's `MonitorEnter` so a thread blocked there
    /// lists the callee at bci 0 (HotSpot's `lock_method` runs with the
    /// callee's frame built). Taken (reset) by that one map.
    pub(in crate::ir_lower) sp_chain_pc_override: Option<usize>,
    /// NPE trap sites this lowering described, becoming
    /// `CompiledMethod::npe_trap_map`. Empty ⇒ the artifact carries the same
    /// empty map every optimizing-tier body carried before 2026-09-11, and a
    /// null receiver in it raises an UNMESSAGED `NullPointerException` exactly
    /// as it did then. See [`Self::record_npe_trap_site`].
    pub(in crate::ir_lower) npe_trap_sites: Vec<(u32, crate::x64::NpeTrapSite)>,
    /// Monotonic id for [`Self::npe_trap_sites`]. Per-ARTIFACT, like the
    /// single-pass backend's — `NpeTrapMap::get` searches one map.
    pub(in crate::ir_lower) npe_trap_next_id: u32,

    // ── Linear-scan register read cache ──────────────────────────────
    //
    // All four are empty / zero unless `CRATONVM_JIT_IR_LINEAR_SCAN` is on, in
    // which case `lower_inner_with_array_list` installs the plan after
    // construction. See the section on [`plan_register_residency`].
    /// `reg_of[id]` = the XMM number [`plan_register_residency`] gave node
    /// `id`'s value for its whole life. Empty when the path is off.
    pub(in crate::ir_lower) reg_of: Vec<Option<u8>>,
    /// `reg_live[id]` = the DEFINITION site for `id` has actually run and
    /// copied the value into `reg_of[id]`.
    ///
    /// This is the safety interlock, not bookkeeping. A read consults
    /// `reg_live`, never `reg_of` directly, so a definition arm this wave did
    /// not convert can only cost an optimization — never produce a read of a
    /// register that was never written. Adding a converted definition site is
    /// therefore additive; forgetting one is not a correctness event.
    pub(in crate::ir_lower) reg_live: Vec<bool>,
    /// Per back-edge block, the FP values its poll's slow path reloads from
    /// their home words after the call (`RegResidency::poll_fp_reloads`).
    /// Empty when the path is off.
    pub(in crate::ir_lower) poll_fp_reloads: HashMap<usize, Vec<NodeId>>,
    /// `gp_reg_of[id]` = the GENERAL-PURPOSE register
    /// [`plan_register_residency`] gave node `id` for its whole life. The
    /// integer twin of `reg_of`; empty when the path is off.
    pub(in crate::ir_lower) gp_reg_of: Vec<Option<u8>>,
    /// The same safety interlock as `reg_live`, for the GP file. A read
    /// consults this and never `gp_reg_of` directly, so an unconverted
    /// definition arm costs an optimization and can never produce a read of a
    /// register nothing wrote.
    pub(in crate::ir_lower) gp_reg_live: Vec<bool>,
    /// `gp_reg_owner[r]` = the node whose value GP register `r` currently
    /// holds, or `None` before anything published into it.
    ///
    /// `gp_reg_live` is indexed by NODE, so on its own it can only say "node
    /// `id` was published at some point" -- never "and nobody has overwritten
    /// its register since". The publish sites' shared comment argued that
    /// nothing could ("the allocator gives a register to one value at a time
    /// over its live range"), and that is true of two values whose live ranges
    /// overlap. It is NOT true across an edge: a phi's live range BEGINS at the
    /// edge, so the allocator may hand it the register of a value whose range
    /// ENDS there -- and `emit_phi_copies` reads the source values and
    /// publishes the phis into the SAME sequence of instructions.
    ///
    /// The resulting hazard is a wrong value, not a crash. `resolve_parallel_copy`
    /// orders the edge's copies so every source SLOT is read before it is
    /// written, and `emit_copy_op`'s comment extends that to registers on the
    /// grounds that "a register and its home word go stale at the same point".
    /// They do not, when the register belongs to one node and the word to
    /// another: an earlier copy's `mov <phi's reg>, rax` publish clobbers a
    /// register a later copy still reads its own source out of, and
    /// `resident_gpr` -- consulting only the per-node bit -- hands that register
    /// back as if it still held the old value.
    ///
    /// MEASURED (2026-09-05, `perf/ir-defaults-on-20260905` defaults): Mockito's
    /// inline mock maker produced class bytes whose forward branch operands were
    /// left as ASM's `ff ff` placeholder -- `Label.resolve` never patched them,
    /// because `Label.addForwardReference`'s `forwardReferences[0]` count read
    /// back wrong -- so CratonVM's verifier rejected the retransformation with
    /// `branch at offset N targets N-1, which is not an instruction boundary`
    /// and every `mock()` of a class failed with `MockitoException: Could not
    /// modify all classes`. 4/4 runs of
    /// `GrpcChannelBuilderCustomizersTests` under the defaults, 0/8 with either
    /// `CRATONVM_JIT_IR_PHI_COPY_REGS=0` (removes the publish) or
    /// `CRATONVM_JIT_LS_CARRY_RELIEF=0` (moves the allocation off the shape).
    ///
    /// Sized by the architectural register file, not by the node count: it is a
    /// map from PHYSICAL register to owner.
    pub(in crate::ir_lower) gp_reg_owner: [Option<NodeId>; 16],
    /// Registers whose current owner was published by a zero-extending
    /// 32-bit write in this block. Homes and edge copies have no such claim.
    pub(in crate::ir_lower) gp_zx32_mask: u16,
    /// JIT round 25 (item 1b, `CRATONVM_JIT_IR_LS_CALLER_SAVED`): some value
    /// of this plan is assigned a CALLER-saved register
    /// ([`IR_LS_CALLER_SAVED_GPRS`]). Set once by
    /// [`Self::install_caller_saved_residency`]; while it is `false`,
    /// [`Self::invalidate_caller_saved_residency`] is a no-op.
    pub(in crate::ir_lower) ls_caller_saved_on: bool,
    /// Values of this plan in a caller-saved register (diagnostic only).
    pub(in crate::ir_lower) ls_caller_saved_values: usize,
    /// `gp_load_value` reads of a non-constant value from its HOME word, and
    /// from a register (round 25, item 1b; diagnostic only -- the
    /// `ir-ls-gp` line under `CRATONVM_DBG_JITC`).
    pub(in crate::ir_lower) gp_home_reloads: usize,
    pub(in crate::ir_lower) gp_reg_reads: usize,
    /// `gp_home_reloads` split by cause (round 25 wave 2, diagnostic only): a
    /// reference (no GP residency for `Ref` unless
    /// `CRATONVM_JIT_IR_REF_RESIDENCY`), a primitive with an assigned register
    /// that was not live at the read (not yet published, invalidated, or
    /// evicted), and a primitive with no register at all.
    pub(in crate::ir_lower) gp_home_reloads_ref: usize,
    pub(in crate::ir_lower) gp_home_reloads_assigned: usize,
    pub(in crate::ir_lower) gp_home_reloads_unassigned: usize,
    /// Round 25 wave 2: `after -> values` -- once `after` is lowered, reload
    /// each value from its home into its caller-saved register
    /// ([`Self::republish_caller_saved_after`]).
    pub(in crate::ir_lower) cs_republish_after: HashMap<NodeId, Vec<NodeId>>,
    /// Reloads that table emitted, and the ones it skipped because a carry
    /// out of `after` needed nothing emitted before its consumer.
    pub(in crate::ir_lower) cs_republishes: usize,
    pub(in crate::ir_lower) cs_republish_skipped: usize,

    // ── Split residency: the position-aware half of the cache ────────
    //
    // All six are empty / false unless `CRATONVM_JIT_IR_LS_SPLITS` is on AND
    // `plan_register_residency` found a value it could place position by
    // position. See [`LsSplitPlan`] for the whole argument; what matters here
    // is that these are the ONLY things that make `reg_of` / `gp_reg_of` change
    // during an emission, and that everything which reads those two vectors as
    // a property of the WHOLE method reads a snapshot instead (below).
    /// `ls_at[pos]` = the transitions to materialise immediately before the
    /// node at linear position `pos`. Empty at every position the plan left
    /// alone, which is nearly all of them.
    pub(in crate::ir_lower) ls_at: Vec<Vec<LsTransition>>,
    /// `ls_block_start[b]` = the first linear position in block `b`, so
    /// `lower_block` can name the position it has reached without carrying the
    /// allocator's `LiveModel` around. Reproduced from the schedule with the
    /// formula `build_live_model` uses, and `plan_register_residency` has
    /// already refused the method if the two disagree about the total.
    pub(in crate::ir_lower) ls_block_start: Vec<usize>,
    /// `ls_managed[id]` = this value's residency is driven by `ls_at` rather
    /// than fixed for its whole live range.
    ///
    /// Read by `set_residency` to withhold two things from exactly this
    /// population: a dropped home (the transitions LOAD from the home word, so
    /// it must be written) and a deopt frame that names the register (the
    /// register is only right at some positions, and a frame state cannot say
    /// which).
    pub(in crate::ir_lower) ls_managed: Vec<bool>,
    /// The register each value's DEFINITION publishes into, snapshot at
    /// `set_residency` before any transition has moved anything.
    ///
    /// `(gp, xmm)`. `ls_split_registers` used to take this from
    /// `assigned_gpr`/`assigned_xmm`, which is the register the value holds
    /// RIGHT NOW — so once a transition had run, the register the value
    /// started in was no longer reported as one it passes through. That is
    /// benign for the save set, which is built before any transition, and not
    /// benign for the other caller: the exclusive-ownership poison decides
    /// which registers may be named in a frame state, and a register that
    /// dropped out of the answer would be called exclusively owned and could
    /// be named in a deopt frame while a managed value was still visiting it.
    ///
    /// Snapshotting the origin makes the answer independent of when it is
    /// asked, which is what its own doc comment ("every register a managed
    /// value passes through") already promised.
    pub(in crate::ir_lower) ls_origin: (Vec<Option<u8>>, Vec<Option<u8>>),
    /// The registers this frame owes its caller, SNAPSHOT at `set_residency`.
    ///
    /// The prologue's save set and every exit's restore set must be the same
    /// set. Before split residency they were both derived from `reg_of` /
    /// `gp_reg_of`, which was safe only because those never changed after
    /// installation — and split residency is precisely the thing that makes
    /// them change, so a prologue that saved RBX could be followed by an
    /// epilogue that no longer saw RBX in `gp_reg_of` and left the caller's
    /// RBX destroyed. The snapshot covers every register the plan can EVER
    /// hand out, including every register a managed value passes through.
    pub(in crate::ir_lower) ls_saved_fp: [bool; 16],
    pub(in crate::ir_lower) ls_saved_gp: [bool; 16],
    /// Transitions actually emitted, by kind: `[load, remat, move, drop]`.
    /// `drop` emits no instruction; it is counted because a plan whose drops
    /// never fire is a plan whose values were never in a register.
    pub(in crate::ir_lower) ls_split_emitted: [usize; 4],
    /// The CFG-edge resolution copies of the split plan (round 9 wave 6), and
    /// whether each edge's copies have been emitted at least once -- an edge
    /// whose copies never were is a compile to refuse, which
    /// `lower_inner_attempt` does after the last block. Both empty unless
    /// `CRATONVM_JIT_IR_LS_EDGE_RESOLUTION` admitted something. See
    /// [`LsEdgeCopies`].
    pub(in crate::ir_lower) ls_edge_copies: Vec<LsEdgeCopies>,
    pub(in crate::ir_lower) ls_edge_copies_done: Vec<bool>,
    /// ENGAGEMENT: edge-resolution loads emitted.
    pub(in crate::ir_lower) ls_edge_loads: usize,
    /// Edge copies that read their source out of a register, and edge copies
    /// that published their phi's register from RAX. Both are ENGAGEMENT
    /// counts: a zero says the wiring never fired, which is a different
    /// finding from "it fired and did not pay".
    pub(in crate::ir_lower) phi_copy_reg_reads: usize,
    /// Round 16 wave 3 (lane codegen, CG16-7): constant phi sources read as
    /// an immediate rather than from their home word.
    pub(in crate::ir_lower) phi_copy_const_imms: usize,
    /// Round 16 wave 4 (lane codegen, CG16-8): while the method prologue zeroes
    /// its frame words, `emit_zero_frame_slot` stores RAX (zeroed once per
    /// run) instead of an `imm32`. Set only inside [`Self::emit_prologue`].
    pub(in crate::ir_lower) prologue_zero_rax: bool,
    /// Round 16 wave 6 (lane codegen, CG16-10): `emit_zero_frame_sse` cleared
    /// this activation's whole frame in the prologue.
    pub(in crate::ir_lower) frame_prezeroed: bool,
    /// Round 16 wave 7 (lane frames): the words of this frame the prologue's
    /// fresh-frame clear LEFT OUT (`fresh_frame_clear_skip`), indexed by word
    /// `w` = `[rbp - 8(w + 1)]`; empty when it left out none. A per-word zero
    /// store onto such a word is emitted even on a pre-zeroed frame.
    pub(in crate::ir_lower) fresh_clear_skip: Vec<bool>,
    /// Set from the prologue's first per-word zero store to its end: the
    /// stretch in which a store of zero onto a pre-zeroed frame is skipped.
    pub(in crate::ir_lower) prologue_zero_stretch: bool,
    /// The buffer offset right after the last RAX-zero store: RAX is still
    /// zero there and only there (nothing else has been emitted since).
    pub(in crate::ir_lower) zero_rax_end: Option<usize>,
    /// [`ir_home_reload_forward_enabled`] for this compile (and no MIR lane,
    /// whose byte comparison would then see two programs).
    pub(in crate::ir_lower) home_reload_forward_on: bool,
    /// JIT rx27 wave 8 (lane regalloc, [`ir_operand_in_place_enabled`]): this
    /// lowering reads a compare's or an ALU op's second operand where it
    /// lives (register, home word, immediate) instead of loading it into
    /// RCX. Only reads change.
    pub(in crate::ir_lower) operand_in_place_on: bool,
    /// Census: compares and ALU ops that read an operand in place under it.
    pub(in crate::ir_lower) operand_in_place_cmps: usize,
    pub(in crate::ir_lower) operand_in_place_alus: usize,
    /// The last `MOV [rbp - off], reg` of [`Self::store_rax`] /
    /// [`Self::store_abi_reg`]: `(reg, off, start, end)`. A frame load of the
    /// same word emitted at `end` reads what `reg` already holds, on the
    /// fall-through path.
    pub(in crate::ir_lower) rax_home_mirror: Option<(u8, i32, usize, usize)>,
    /// JIT rx27 wave 9 (lane regalloc, [`ir_publish_reload_elide_enabled`]):
    /// this lowering skips the `MOV RAX, reg` read of a value whose register
    /// publish `MOV reg, RAX` (and at most its own home store) is the last
    /// thing emitted.
    pub(in crate::ir_lower) publish_reload_elide_on: bool,
    /// The last register publish of a definition: `(id, reg, end)`. RAX
    /// equals `reg` at `end`, on the fall-through path.
    pub(in crate::ir_lower) rax_reg_mirror: Option<(NodeId, u8, usize)>,
    /// Every read skipped under it: `(code offset, value)`, checked after the
    /// body by [`Self::verify_publish_reload_elides`] (no branch lands there).
    pub(in crate::ir_lower) publish_reload_elides: Vec<(usize, NodeId)>,
    /// Per value: a read of it was skipped (the retry's `note_elide_refusal`).
    pub(in crate::ir_lower) publish_reload_elided: Vec<bool>,
    /// Per value: an earlier attempt of this compile was refused for it.
    pub(in crate::ir_lower) publish_reload_excluded: Vec<bool>,
    /// Census: reads refused after the body (a branch may land on them).
    pub(in crate::ir_lower) publish_reload_refused: usize,
    /// The frame loads emitted right behind such a store, rewritten after the
    /// body by [`Self::apply_home_reload_forwards`].
    pub(in crate::ir_lower) home_reload_forwards: Vec<HomeReloadForward>,
    pub(in crate::ir_lower) phi_copy_reg_publishes: usize,
    /// Publishes DEFERRED to the end of their edge because the phi's register
    /// is also a source's register on that edge (see `emit_phi_copies`). A
    /// non-zero here is the wrong-code hazard being taken off the table, not a
    /// missed optimization: the value is still published, one word-load later.
    pub(in crate::ir_lower) phi_copy_publish_deferred: usize,
    /// Trailing residency publishes SKIPPED because the phi's home word was
    /// dropped and its register already holds the value, and the same site
    /// REFUSED because neither location was readable. See `emit_phi_copies`.
    pub(in crate::ir_lower) phi_home_publish_skipped: usize,
    pub(in crate::ir_lower) phi_home_publish_refused: usize,
    /// FP phis published at an edge from their source's register rather than
    /// the home word (`CRATONVM_JIT_IR_LOOP_CARRIED_FP_REGS`).
    pub(in crate::ir_lower) fp_phi_reg_publishes: usize,
    /// FP phi homes an edge copy wrote from the source's register rather than
    /// through RAX (the same switch).
    pub(in crate::ir_lower) fp_phi_reg_stores: usize,
    /// FP binops computed in their result's register
    /// ([`Self::fp_binop_value`]).
    pub(in crate::ir_lower) fp_in_place_ops: usize,
    /// The value `lower_data_node` is currently emitting, so `store_rax` can
    /// tell the one store that writes a value's OWN home word from the many
    /// that write an argument stage word, a shadow-stack word, or somebody
    /// else's slot. `None` everywhere outside a data node's lowering, which is
    /// what keeps `emit_copy_op`'s store out of this.
    pub(in crate::ir_lower) cur_def: Option<NodeId>,
    /// JIT r23 wave 11b (lane L5, `CRATONVM_JIT_IR_SYNTHETIC_STORE_FIELD_ROW`):
    /// the per-NODE compact row (`Graph::field_rows`) of the `Op::Store` being
    /// lowered, set by its arm around the inline store emitters and `None`
    /// everywhere else. A store at a `Graph::synthetic_field_access_pcs` pc
    /// (the VH fresh-field `set*`, `IrBuilder::try_vh_fresh_field`) has no
    /// per-pc row; without its node row it took `jit_putfield_object` /
    /// `jit_putfield_*` on every execution.
    pub(in crate::ir_lower) synthetic_store_row: Option<(u32, bool, u8)>,
    /// Whether that store has happened and published the register.
    pub(in crate::ir_lower) cur_def_published: bool,
    /// ENGAGEMENT: registers published from RAX at the definition rather than
    /// reloaded from the home word, and home stores skipped there.
    pub(in crate::ir_lower) reg_publishes_at_def: usize,
    pub(in crate::ir_lower) homes_dropped_at_def: usize,
    /// `carry_of[id]` = `(register, consumer)`: this single-use value reaches
    /// its one consumer in `register` and never touches its home word. Empty
    /// unless [`ir_carry_single_use_enabled`]. See that function for why this
    /// is not a residency decision.
    pub(in crate::ir_lower) carry_of: Vec<Option<(u8, NodeId)>>,
    /// The carry in flight: `(producer, consumer, register, buffer position
    /// when it started)`. At most one is ever live, because a carry is planned
    /// only between ADJACENT nodes.
    pub(in crate::ir_lower) live_carry: Option<(NodeId, NodeId, u8, usize)>,
    /// The DEFERRED carry: a value sitting in RCX for a consumer that is two
    /// nodes away, not one.
    ///
    /// `(producer, consumer, intervening nodes still permitted)`.
    ///
    /// [`Self::live_carry`] is the adjacent carry and can be either register;
    /// this is the second slot, and it is RCX-only by construction — the
    /// consumer reads its first operand from RAX, which is where the ADJACENT
    /// producer left it, so the only value that can still be in flight across
    /// an arm is the second operand. The allowance is 1 and is spent by the
    /// arm in between, whose op must satisfy [`op_preserves_rcx`]; running out
    /// of it refuses the compile rather than reading a register something else
    /// has since written.
    pub(in crate::ir_lower) deferred_rcx: Option<(NodeId, NodeId, u8)>,
    /// The code position right after [`Self::gp_load_index_rcx`] loaded an
    /// array index into RCX with a zero-extending 32-bit form (MX-B2); the
    /// bounds guard skips its `MOV ECX, ECX` only at exactly this position.
    pub(in crate::ir_lower) index_zx_at: Option<usize>,
    /// The vector pre-loops of this compile (`emit_vector_preloop`), planned on
    /// first use; `None` until then.
    pub(in crate::ir_lower) vec_plans: Option<Vec<crate::ir_vector_loop::VecLoopPlan>>,
    /// How many vector pre-loops were emitted.
    pub(in crate::ir_lower) vec_loops_emitted: usize,
    /// Values `plan_carries` chose to carry in the DEFERRED slot, so
    /// `store_rax` can tell the two apart at the home write.
    pub(in crate::ir_lower) carry_deferred: Vec<bool>,
    /// Deferred carries planned, taken and read — engagement for the census.
    pub(in crate::ir_lower) carry_deferred_planned: usize,
    pub(in crate::ir_lower) carry_deferred_read: usize,
    /// Why a `[input1, input0, cons]` triple was NOT deferred, per cause:
    /// [0 no-RAX-carry-on-the-middle, 1 producer-already-carrying,
    ///  2 producer-multi-use, 3 producer-type-or-arm, 4 producer-resident,
    ///  5 operand-position, 6 consumer, 7 middle-arm-can-write-RCX].
    ///
    /// A bare `deferred=N/M` says how often the shape was TAKEN and nothing
    /// about how often it was there, and that is the distinction
    /// `c2-one-carry-slot-is-the-frame-traffic-ceiling-FIXED-20260910.md`
    /// closed on — so the causes are split finely enough to name the next
    /// increment rather than merely to record that there was one.
    ///
    /// Cause 0 is every other triple of scheduled nodes and is not a candidate;
    /// [`Self::deferred_candidates`] is the denominator the rest divide into.
    /// `deferred_mid_foldable` splits cause 7: how many of those middle arms
    /// fold their own second operand into an immediate and so never reach
    /// `gp_load_value(RCX, ..)` at all.
    pub(in crate::ir_lower) deferred_skips: [usize; 8],
    pub(in crate::ir_lower) deferred_mid_foldable: usize,
    /// Windows in which the consumer already takes its FIRST operand in RAX —
    /// the shape the second slot exists for, and the only honest denominator
    /// for the causes above.
    pub(in crate::ir_lower) deferred_candidates: usize,
    /// Writes to RCX this lowering has emitted, through every route in this
    /// file that reaches the register. Snapshotted around each node so a
    /// deferred carry's survival is OBSERVED rather than predicted — see
    /// [`Self::note_rcx_written`].
    pub(in crate::ir_lower) rcx_writes: usize,
    /// Fused compares that read both operands where they already were.
    pub(in crate::ir_lower) cmp_in_place: usize,
    /// Fused compares that read the second operand straight out of its frame
    /// slot instead of loading it into RCX first.
    pub(in crate::ir_lower) cmp_in_place_frame: usize,
    /// Fused compares against a CONSTANT second operand, by where the first
    /// operand was read from: its register, and its frame slot.
    ///
    /// `i < 100` is the shape, and it is the most common comparison in Java.
    /// Both forms fold the constant into the instruction instead of
    /// materialising it in RCX first.
    pub(in crate::ir_lower) cmp_imm: usize,
    pub(in crate::ir_lower) cmp_imm_frame: usize,
    /// `x + k` / `x - k` emitted as one `LEA` into the result's own register
    /// instead of load-accumulate-publish, and — the common case — as one
    /// `LEA` into the accumulator instead of a `MOV` and an `ADD`.
    pub(in crate::ir_lower) add_lea: usize,
    pub(in crate::ir_lower) add_lea_acc: usize,
    /// ENGAGEMENT, and its fail-closed counterpart. A refusal is not a
    /// miscompile — the value's home is `home_dropped`, so the fallback read
    /// refuses the compile as well — but it means the contract this planned
    /// under did not hold, which is a bug in the plan, not in the emission.
    pub(in crate::ir_lower) carries_taken: usize,
    pub(in crate::ir_lower) carries_read: usize,
    pub(in crate::ir_lower) carries_refused: usize,
    pub(in crate::ir_lower) carry_stores_dropped: usize,
    /// JIT round 25 item 1a ([`ir_elide_unnamed_homes_enabled`]): this
    /// attempt may elide unnamed homes. Off when the switch is off, under a
    /// MIR mode, and on the keep-everything retry (`keep_deopt_image`), so a
    /// misprediction costs one more lowering, never the method.
    pub(in crate::ir_lower) elide_unnamed_homes_on: bool,
    /// [`Self::deopt_named_reachable`] as this round's planning reads it:
    /// the residency plan's copy, or computed in `prepare_fusion_tables` when
    /// no plan was installed. Empty while the switch is off.
    pub(in crate::ir_lower) elide_reach: Vec<bool>,
    /// Values some frame state holds in a local or a monitor (not only on the
    /// operand stack); neither widening takes one. Empty while the switch is
    /// off.
    pub(in crate::ir_lower) elide_held: Vec<bool>,
    /// `tail_carry_of[id] = Some(consumer)`: `id`'s definition-time home store
    /// is not emitted, and its one consumer -- the next scheduled node, or the
    /// block's terminator -- reads it in RAX with nothing emitted in between.
    /// Planned by [`Self::plan_tail_carries`] for producers the ordinary carry
    /// cannot take (a call result, a field load, a value read by an `If` or a
    /// `Return`). Every such value's home is `home_dropped`, so a reader the
    /// plan did not foresee refuses rather than reads the unwritten word.
    pub(in crate::ir_lower) tail_carry_of: Vec<Option<NodeId>>,
    /// Whether `store_rax` has already elided `id`'s home store: a second home
    /// write of the same value (a producer with more than one route to its
    /// home) refuses, because the first route's register is not proven to
    /// reach the consumer.
    pub(in crate::ir_lower) tail_carry_seen: Vec<bool>,
    /// The tail carry in flight: `(producer, consumer, buffer position of the
    /// store that was not emitted)`.
    pub(in crate::ir_lower) tail_carry: Option<(NodeId, NodeId, usize)>,
    /// The terminator `lower_terminator` is lowering, so a tail carry into an
    /// `If` / `Return` can be told from a stray read (`cur_def` is `None`
    /// there).
    pub(in crate::ir_lower) cur_term: Option<NodeId>,
    /// Code offsets at which a tail carry was read -- where the elided store
    /// would have ended. No instruction of the finished body may branch to
    /// one ([`Self::verify_tail_carry_points`]).
    pub(in crate::ir_lower) tail_carry_points: Vec<(usize, NodeId)>,
    /// JIT round 25 item 1a (wave 2): `site_named[id]` -- a register-resident
    /// value whose home store is dropped because every frame state that can
    /// name it at a reachable bci is resolved at a deopt SITE, where
    /// [`Self::site_named_frame_value`] names its register after checking the
    /// allocator's liveness there ([`Self::plan_site_named_homes`]).
    pub(in crate::ir_lower) site_named: Vec<bool>,
    pub(in crate::ir_lower) site_named_count: usize,
    /// The allocator's liveness facts the site check reads; `None` unless the
    /// switch is on and the plan was installed.
    pub(in crate::ir_lower) site_live: Option<SiteLive>,
    /// Set while `build_deopt_points` resolves the table points (no site).
    pub(in crate::ir_lower) building_table_points: std::cell::Cell<bool>,
    /// Mask of reference registers refreshed after a collecting site when a
    /// post-site frame state is resolved before `invalidate_ref_residency`.
    pub(in crate::ir_lower) post_collect_ref_mask: std::cell::Cell<Option<u16>>,
    /// The bci of the table point being resolved, while one is.
    pub(in crate::ir_lower) table_point_bci: std::cell::Cell<Option<usize>>,
    /// Wave 3 ([`ir_elide_homes_spill_at_inspection_enabled`]): store a
    /// site-named value's home at the inspection points that name it, not at
    /// its definition ([`Self::emit_inspection_spills`]).
    pub(in crate::ir_lower) spill_at_inspection_on: bool,
    /// The inspection bcis of this plan, and per bci the site-named values a
    /// frame state there names.
    pub(in crate::ir_lower) site_spill_bcis: std::collections::HashSet<usize>,
    pub(in crate::ir_lower) site_spill_named: HashMap<usize, Vec<NodeId>>,
    /// Per value, the blocks an inspection spill of it was emitted in.
    pub(in crate::ir_lower) site_spills: HashMap<NodeId, Vec<usize>>,
    /// The block `lower_block` is lowering.
    pub(in crate::ir_lower) cur_lower_block: usize,
    /// ENGAGEMENT: inspection spills emitted, and skipped because one already
    /// reached the site on every path.
    pub(in crate::ir_lower) inspection_spills_emitted: usize,
    pub(in crate::ir_lower) inspection_spills_deduped: usize,
    /// JIT round 25 wave 6, lane W6-B ([`ir_site_named_loop_values_enabled`]):
    /// this attempt site-names loop values (headers liveness-checked, phis
    /// admitted, back-edge spills in the poll's slow path).
    pub(in crate::ir_lower) site_named_loop_on: bool,
    /// W6-B: the `(register, home)` stores the next back-edge poll owes in
    /// its SLOW path, queued by `emit_inspection_spills` (terminator) and
    /// `emit_fallthrough_poll_spills`, taken by `emit_safepoint_poll`.
    pub(in crate::ir_lower) poll_slow_spills: Vec<(u8, i32)>,
    /// W6-B: an outlined poll's slow-path stores, keyed by its `JNZ`'s rel32
    /// field (the `slow_patch` of `outlined_polls`).
    pub(in crate::ir_lower) outlined_poll_spills: HashMap<usize, Vec<(u8, i32)>>,
    /// Second `JNZ` patch site (`stw_set`) for outlined polls that also test
    /// `owed_latch`, keyed by the poll's primary `slow_patch` in `outlined_polls`.
    pub(in crate::ir_lower) outlined_poll_stw_patches: HashMap<usize, usize>,
    /// j27 wave 5 (lane UNROLLRES, `CRATONVM_JIT_IR_PAGE_POLL`): the page-form
    /// polls' `(site, recovery pc)` buffer offsets, registered in the
    /// implicit-null table with the body (`ir_page_poll`).
    pub(in crate::ir_lower) page_poll_sites: Vec<(usize, usize)>,
    /// The outlined page-form polls whose slow block `emit_outlined_polls`
    /// has yet to lay down, keyed (like `outlined_poll_spills`) by the site.
    pub(in crate::ir_lower) page_polls_outlined: Vec<usize>,
    /// Outlined page-form polls paired with a `JNZ .slow` for `owed_latch`:
    /// `(slow_patch, page_site)`.
    pub(in crate::ir_lower) page_polls_outlined_paired: Vec<(usize, usize)>,
    /// Per-compile CHA leaf class IDs (non-final classes with zero loaded
    /// subclasses at compile time).
    pub(in crate::ir_lower) cha_leaf_classes: rustc_hash::FxHashSet<u32>,
    /// W6-B: set while the fall-through back-edge poll of this block is being
    /// emitted, so a site resolved inside it (its mode exit) is asked at the
    /// block's outgoing edge ([`Self::site_location_of`]).
    pub(in crate::ir_lower) fallthrough_poll_block: Option<usize>,
    /// W6-B ENGAGEMENT: slow-path stores emitted, and phis site-named.
    pub(in crate::ir_lower) poll_slow_spills_emitted: usize,
    pub(in crate::ir_lower) site_named_phis: usize,
    /// JIT round 25 wave 7, lane W7-A (`CRATONVM_JIT_IR_REG_ALLOC`): this
    /// attempt runs the IR register-allocator increment (see
    /// `r25_w7a_reg_alloc`). Off on the keep-everything retry and under a MIR
    /// mode.
    pub(in crate::ir_lower) reg_alloc_on: bool,
    /// W7-A ENGAGEMENT: coalesced back-edge phi sources this plan site-named.
    pub(in crate::ir_lower) reg_alloc_coalesced_named: usize,
    /// W8-4 (increment 2): the last register publish of a dropped home under
    /// the allocator, `(node, position after the publish)`
    /// (`publish_def_at_store`), and the multi-route definitions to verify
    /// after the body (`r25_w7a_reg_alloc`, `reg_alloc_check_routed_def`).
    pub(in crate::ir_lower) reg_alloc_publish_at: Option<(NodeId, usize)>,
    pub(in crate::ir_lower) reg_alloc_routed: Vec<(NodeId, usize, usize)>,
    /// W9-2 ENGAGEMENT: ALU definitions emitted straight into their register
    /// (`r25_w7a_reg_alloc`, `reg_alloc_direct_alu`).
    pub(in crate::ir_lower) reg_alloc_direct_defs: usize,
    /// W7 increment 6 ENGAGEMENT: phi edge copies staged in a kept-home phi's
    /// own register (`r25_w7a_reg_alloc`, `reg_alloc_stages_kept_phi`).
    pub(in crate::ir_lower) reg_alloc_kept_phi_stages: usize,
    /// W6-B census (`CRATONVM_DBG_JITC` only, else empty): per node, why its
    /// home store is kept (`HK_*`), and the emitted home stores per reason,
    /// static and weighted by the emitting block's frequency.
    pub(in crate::ir_lower) home_keep_why: Vec<u8>,
    pub(in crate::ir_lower) home_why_static: [usize; HK_N],
    pub(in crate::ir_lower) home_why_weighted: [f64; HK_N],
    /// W6-B census: phi edge copies that wrote the phi's home.
    pub(in crate::ir_lower) phi_edge_home_stores: usize,
    pub(in crate::ir_lower) phi_edge_home_weighted: f64,
    /// Values this compile's earlier attempts refused an elision of
    /// (`ElideRetryScope`); none of this round's plans takes one.
    pub(in crate::ir_lower) elide_excluded: Vec<bool>,
    /// Ends of the post-call exit sites a tail carry was moved past
    /// (`lower_data_node_tracked`), for the census.
    pub(in crate::ir_lower) tail_carry_site_ends: Vec<usize>,
    /// The rel32 fields of the post-call exit stubs' `JZ` back to their
    /// site's end (`emit_post_call_exit_stubs`): branches that may reach a
    /// tail carry's read and leave RAX as it was.
    pub(in crate::ir_lower) post_call_exit_backs: Vec<usize>,
    /// `Cmp` nodes fused into their `If` although a frame state names them,
    /// because no REACHABLE deopt does ([`Self::deopt_named_reachable`]);
    /// [`Self::frame_value_for`] refuses one a by-bci point would describe.
    pub(in crate::ir_lower) fused_cmp_unnamed: Vec<bool>,
    /// ENGAGEMENT for the `CRATONVM_DBG_JITC` `ir-home-stores` line: def home
    /// stores `store_rax` emitted, tail carries planned / elided / refused,
    /// and compares fused under the widened rule.
    pub(in crate::ir_lower) def_home_stores: usize,
    pub(in crate::ir_lower) tail_carries_planned: usize,
    /// Diagnostics for the `ir-home-stores` line: tail carries not taken
    /// because an inline call prefix joins after the store
    /// (`tail_prefix_kept`), or because the consumer is a back-edge `If`
    /// (`tail_backedge_skipped`).
    pub(in crate::ir_lower) tail_prefix_kept: usize,
    pub(in crate::ir_lower) tail_backedge_skipped: usize,
    /// JIT round 26 (the item-1a multi-route residual): inline call prefixes
    /// whose hit edge was proven to carry the result in RAX
    /// ([`Self::prefix_hit_proves_rax`]): `(the hit JMP's opcode position, its
    /// rel32 operand position, the producer)`. The operand may reach the tail
    /// carry's read (`verify_tail_carry_points` exempts it); nothing else may
    /// branch to the JMP.
    pub(in crate::ir_lower) prefix_rax_proofs: Vec<(usize, usize, NodeId)>,
    /// Diagnostics: compares a frame state names that the unnamed fusion
    /// refused, by reason (reachable deopt, held in a local, excluded by a
    /// retry, back-edge branch).
    pub(in crate::ir_lower) cmp_unnamed_refused: [usize; 4],
    pub(in crate::ir_lower) tail_stores_elided: usize,
    pub(in crate::ir_lower) tail_carries_refused: usize,
    pub(in crate::ir_lower) fused_cmp_unnamed_count: usize,
    /// Memoised [`Self::graph_cannot_deopt`].
    pub(in crate::ir_lower) graph_trap_free: std::cell::OnceCell<bool>,
    /// nio21 wave 7 (lane L5): the masked-access proofs of this (immutable)
    /// graph, computed once (`ir_optimize::MaskGuardProofs`); `None` when
    /// `CRATONVM_JIT_IR_MASK_GUARD_ELIDE=0`. Read by
    /// [`Self::emit_array_null_bounds_guards_for`] and
    /// [`Self::node_cannot_deopt_here`], so the emission and the prediction
    /// keep reading one predicate.
    pub(in crate::ir_lower) mask_proofs:
        std::cell::OnceCell<Option<crate::ir_optimize::MaskGuardProofs>>,
    /// ENGAGEMENT: homes dropped because every frame state naming them is
    /// unreachable, and deopt points skipped for the same reason — including
    /// (round 11 wave 9) a table point no site publishes beside one that is
    /// (`snapshots_no_site_publishes_beside_one_that_does`).
    pub(in crate::ir_lower) unreachable_homes: usize,
    pub(in crate::ir_lower) unreachable_points_skipped: std::cell::Cell<usize>,
    /// ENGAGEMENT: constant operands folded into their ALU instruction.
    pub(in crate::ir_lower) alu_imms_folded: usize,
    /// Why an adjacent pair was not carried, per cause, and how many carried
    /// values still had to write their home because a frame state names them.
    pub(in crate::ir_lower) carry_skips: [usize; 6],
    pub(in crate::ir_lower) carry_named: usize,
    /// `deopt_nameable[id]` — a deopt frame may describe `id` as living in its
    /// register rather than in its home word.
    ///
    /// **The rule is exclusive ownership, and it is deliberately stronger than
    /// "the value is resident".** A register-allocated value owns its register
    /// only over its LIVE RANGE, and `plan_register_residency` releases the
    /// deopt pins, so a bytecode local can still be named by a frame state long
    /// after its last IR use — by which time the allocator may have given the
    /// register to something else. Naming it then would reconstruct a
    /// confidently wrong value, which is the failure mode this area produces.
    ///
    /// So a value is nameable only when **no other value anywhere in the method
    /// is assigned the same register**. Then the register holds that value from
    /// its definition to the end of the frame, no position mapping between
    /// `graph.safepoints` and the allocator's positions is needed, and the
    /// question "is the register still his at THIS bci" cannot be got wrong.
    pub(in crate::ir_lower) deopt_nameable: Vec<bool>,
    /// Round 16 wave 3 (lane codegen, CG16-6): `phi_coalesced_src[v]` -- `v`
    /// is a loop phi's back-edge source that [`Self::coalesce_back_edge_phi_sources`]
    /// put in the PHI's register. The pair counts as one holder of that
    /// register for `deopt_nameable`: the phi is dead from `v`'s definition
    /// to the edge copy that makes `v` the phi again. Empty unless
    /// `CRATONVM_JIT_IR_PHI_COALESCE_BACK_EDGE`.
    pub(in crate::ir_lower) phi_coalesced_src: Vec<bool>,
    /// How many back-edge sources CG16-6 coalesced in this compile.
    pub(in crate::ir_lower) phi_coalesced: usize,
    /// How many constant homes CG16P-4 dropped in this compile
    /// ([`Self::drop_const_homes`]).
    pub(in crate::ir_lower) const_homes_dropped: usize,
    /// How many ALU ops read their second operand from its register
    /// ([`Self::emit_alu_acc_reg`]).
    pub(in crate::ir_lower) alu_reg_operands: usize,
    /// `home_dropped[id]` — this value's home word is never written, so reading
    /// it would read whatever the last tenant of that frame word left there.
    ///
    /// [`Self::slot_of`] refuses such a read and fails the compile, which costs
    /// coverage (the method runs in a lower tier) and can never be a wrong
    /// answer. That is deliberately the opposite trade from enumerating the
    /// readers into a whitelist: a whitelist that is wrong is a silent
    /// miscompile, and a refusal that is wrong is a census entry.
    pub(in crate::ir_lower) home_dropped: Vec<bool>,
    /// How many stores were dropped, and how many `slot_of` reads refused.
    /// A non-zero refusal count names work: the reader wants converting to
    /// `gp_load_value`. `Cell` because `slot_of` takes `&self`.
    /// `(bci, offset of its entry stub)` for every bci this body can be
    /// entered at. Empty unless [`ir_osr_entry_enabled`].
    pub(in crate::ir_lower) osr_entries: Vec<(u32, u32, u32)>,
    /// `(bci, kind byte per seeded local)` for each entry in `osr_entries`;
    /// published as `CompiledMethod::ir_osr_entry_kinds`.
    pub(in crate::ir_lower) osr_entry_kinds: Vec<(u32, Box<[u8]>)>,
    /// The bcis of `osr_entries` whose stub is an OPAQUE arm (round 11 wave
    /// 17, lane tier); published sorted as
    /// `CompiledMethod::ir_osr_opaque_entry_bcis`, the OSR door's per-entry
    /// bit.
    pub(in crate::ir_lower) osr_opaque_entry_bcis: Vec<u32>,
    /// Why a bci did not get one. A per-CAUSE census, because the four causes
    /// call for four different next steps and a bare count cannot be acted on.
    pub(in crate::ir_lower) osr_refusals: Vec<(&'static str, usize)>,
    pub(in crate::ir_lower) home_stores_dropped: usize,
    pub(in crate::ir_lower) home_read_refusals: std::cell::Cell<usize>,
    /// Interpreter round i2 wave 64 (lane L2,
    /// `i63-L2-proposal-the-lowering-refuses-a-token-slot-operand-read`):
    /// `CRATONVM_JIT_IR_LOWER_TOKEN_READ_GUARD` (or the slot-plan token switch,
    /// which implies it), read once per lowering. See
    /// [`Self::token_read_check`].
    pub(in crate::ir_lower) token_read_guard: bool,
    /// The inputs of the data node being lowered that it names ONLY in a
    /// memory-token slot: `(id, slot, exempt)`. `exempt` = the colouring
    /// pinned the id's home (a φ, a deopt-named value) or a snapshot names it
    /// ([`Self::token_guard_named`]), so a read of it is counted but not
    /// refused. Empty outside a
    /// data node's lowering and whenever the guard is off.
    pub(in crate::ir_lower) token_only_inputs: Vec<(NodeId, usize, bool)>,
    /// Data nodes lowered with a non-empty [`Self::token_only_inputs`].
    pub(in crate::ir_lower) token_read_nodes: usize,
    /// Reads the guard refused (the first one latches the bailout).
    pub(in crate::ir_lower) token_read_refusals: std::cell::Cell<usize>,
    /// Reads of a PINNED token-only input: counted, not refused.
    pub(in crate::ir_lower) token_read_pinned_hits: std::cell::Cell<usize>,
    /// JIT round 19 wave 10 (lane tok10, i64-L2): which lowering scope
    /// installed [`Self::token_only_inputs`] -- a data node's arm, a block
    /// terminator (with its fused compare), or a level-2 MIR tile. Names the
    /// scope on a hit line and in the census; [`TokenScope::Data`] outside
    /// every scope (the set is empty there anyway).
    pub(in crate::ir_lower) token_scope: TokenScope,
    /// The node that installed a `term` / `mir` set (the terminator, the
    /// tile's root): the reader a hit line names. Unused for `data`.
    pub(in crate::ir_lower) token_scope_reader: NodeId,
    /// Per [`TokenScope`]: scopes entered with a non-empty set, refusals,
    /// pinned (exempt) hits. The three un-split counters above are the sums.
    pub(in crate::ir_lower) token_scope_nodes: [usize; 3],
    pub(in crate::ir_lower) token_scope_refusals: std::cell::Cell<[usize; 3]>,
    pub(in crate::ir_lower) token_scope_pinned_hits: std::cell::Cell<[usize; 3]>,
    /// `token_guard_named[id]` = some safepoint snapshot (a by-bci frame, a
    /// chain snapshot, a monitor list) names `id`. Built on the guard's first
    /// node, empty until then and whenever the guard is off. Such an id is
    /// exempt like a pinned one: a deopt describes it from its home or
    /// register at a GC point or guard, which is not an operand read, and its
    /// home keeps (or is widened to) every point that can publish it.
    pub(in crate::ir_lower) token_guard_named: Vec<bool>,
    /// `CRATONVM_JIT_IR_LOWER_TOKEN_READ_GUARD_POINT_RANGE` (opt-in; JIT round
    /// 19 proposal wave 11, lane lv11, TOK10-3), read once per lowering: an id
    /// exempt by identity (pinned, or named by a snapshot) stays exempt only
    /// while its home's colouring range covers the reader's position. See
    /// [`token_exempt_at`].
    pub(in crate::ir_lower) token_guard_point_range: bool,
    /// Identity-exempt token-only inputs the point-range rule took the
    /// exemption from (TOK10-3): set entries, not reads.
    pub(in crate::ir_lower) token_point_stale: std::cell::Cell<usize>,
    /// `CRATONVM_JIT_IR_DEOPT_POINT_RANGE_CHECK` (opt-in; JIT round 19
    /// proposal wave 12, lane lv12, LV11-4), read once per lowering: a
    /// by-bci deopt point resolved in [`Self::build_deopt_points`] must not
    /// name a shareable home past its colouring range ([`point_home_covers`]).
    pub(in crate::ir_lower) deopt_point_range_check: bool,
    /// Values the LV11-4 check found named past their home's range.
    pub(in crate::ir_lower) deopt_point_stale: std::cell::Cell<usize>,
    /// How many frame-state slots were described by a register. Engagement, and
    /// `Cell` because `frame_value_for` takes `&self`.
    pub(in crate::ir_lower) deopt_reg_named: std::cell::Cell<usize>,
    /// Set by [`Self::build_deopt_points`] while it resolves a by-bci point at
    /// a bci this body can NOT transfer from (`transfer_bcis`). There,
    /// [`Self::frame_value_for`] answers a dropped, register-less home with a
    /// quiet `Unsupported` instead of latching `UnallocatedValue`: no deopt
    /// can arrive at that point, so nothing reads the description. At a bci
    /// that CAN transfer the latch stands, and `lower_inner` reads it again
    /// after the points are built (round 9 wave 2,
    /// `ir-bailouts-latched-after-the-latch-check-are-dropped`).
    pub(in crate::ir_lower) quiet_dropped_home: std::cell::Cell<bool>,
    /// Number of input references to each node, over every input of every
    /// node. Filled by `prepare_fusion_tables` before the first block lowers.
    pub(in crate::ir_lower) use_count: Vec<u32>,
    /// `Op::Cmp` nodes whose only use is the `Op::If` terminator consuming
    /// them, lowered as one fused compare-and-branch by that terminator; the
    /// compare's own arm emits nothing (`ir_fused_branch_enabled`).
    pub(in crate::ir_lower) fused_cmp: Vec<bool>,
    /// JIT round 21, lane tcfuse (`CRATONVM_JIT_TYPECHECK_BRANCH_FUSION`,
    /// TCPERF21-2): the inline answers of the `Op::InstanceOf` lowered last,
    /// waiting for the `Op::If` that ends its block: `(cond, one, zero,
    /// one_is_true)` -- the fused compare the `If` branches on, the rel32
    /// operands of the `1` and `0` jumps, and whether a `1` takes the `If`'s
    /// true edge. Set only by [`Lowerer::typecheck_branch_fusion_for`]'s
    /// admission; the `If` arm resolves it, and `lower_block` refuses the
    /// compile if one is left over.
    pub(in crate::ir_lower) typecheck_branch_pending:
        Option<(NodeId, Vec<usize>, Vec<usize>, bool)>,
    /// `Op::LCmp` nodes whose only use is a FUSED `Cmp(cc, lcmp, 0)` (or
    /// `Cmp(cc, 0, lcmp)`): the `If` compares the two `long`s directly
    /// (`CMP r64, r64; Jcc`) and the `lcmp` arm emits nothing -- no
    /// `SETG/SETL/SUB/MOVSXD`, no home word (round 9 wave 3, irlower3; the IR
    /// half of `perf-long-counted-loops-lose-the-optimizing-osr-body`).
    /// Such a node has NO location, so [`Lowerer::frame_value_for`] refuses the
    /// compile if a deopt that can actually happen names it.
    pub(in crate::ir_lower) fused_lcmp: Vec<bool>,
    /// The `fcmp<l|g>` / `dcmp<l|g>` twin of [`Self::fused_lcmp`] (round 9
    /// wave 8, irl8): an `Op::FCmp` whose only use is a FUSED ordering compare
    /// against 0. The `If` emits `UCOMIS; Jcc` with the operand order and
    /// condition [`fcmp_fused_branch`] picks, and the `FCmp` arm emits
    /// nothing. No location either, so [`Lowerer::frame_value_for`] refuses
    /// the compile if a reachable deopt names it.
    pub(in crate::ir_lower) fused_fcmp: Vec<bool>,
    /// JIT rx27 wave 4 (lane regalloc, [`ir_and_zero_test_branch_enabled`]):
    /// an `Op::And` with a constant mask whose only use is a FUSED `== 0` /
    /// `!= 0` compare of an `If`. The `If` emits `TEST x, mask; Jcc` and this
    /// arm emits nothing: no value, no home word. No location either, so
    /// [`Lowerer::frame_value_for`] refuses the compile if a deopt names it
    /// (the plan takes none a snapshot names).
    pub(in crate::ir_lower) fused_test_and: Vec<bool>,
    pub(in crate::ir_lower) fused_i2l: Vec<bool>,
    /// `test_fused_cond[cmp] = Some((x, mask, wide))`: the fused compare of
    /// an `If` that [`Self::plan_and_zero_tests`] answers as `TEST x, mask`.
    pub(in crate::ir_lower) test_fused_cond: Vec<Option<(NodeId, i32, bool)>>,
    /// JIT rx27 wave 6 (lane regalloc, [`ir_and_zero_test_guard_enabled`]):
    /// an `Op::And` answered by its fused `Guard`'s `TEST` that the guard's own
    /// frame state names (the operand a pruned `ifeq` re-reads). It emits
    /// nothing inline; its home word is written in that guard's deopt pad,
    /// recomputed from `x` there ([`PadRemat`]), before the stub reads the
    /// frame. Nothing else reads it ([`Lowerer::plan_and_zero_guard_tests`]).
    pub(in crate::ir_lower) test_and_remat: Vec<bool>,
    /// `test_guard_cond[cmp] = Some((x, mask, wide))`: a fused compare of a
    /// `Guard` answered as `TEST x, mask`.
    pub(in crate::ir_lower) test_guard_cond: Vec<Option<(NodeId, i32, bool)>>,
    /// Per deopt pad (`deopt_pads` patch position): the home word to rebuild
    /// before the pad leaves.
    pub(in crate::ir_lower) deopt_pad_remats: HashMap<usize, PadRemat>,
    /// Guards planned (plain and with a pad remat), for the retry flag.
    pub(in crate::ir_lower) and_zero_guard_planned: usize,
    /// JIT rx27 wave 6b: `If` and-zero tests taken although a frame state
    /// names the `And` (no reachable deopt does), for the retry flag.
    pub(in crate::ir_lower) and_zero_widened: usize,
    /// Nodes named by some safepoint snapshot. Their home word must hold the
    /// value at that bci, so such a compare is never fused away.
    pub(in crate::ir_lower) deopt_named: Vec<bool>,
    /// `deopt_named_reachable[id]` — a deopt that can ACTUALLY HAPPEN names
    /// `id`. See [`Lowerer::compute_deopt_named_reachable`]; this is the
    /// per-value refinement of the whole-body `graph_cannot_deopt`, and it is
    /// what lets a promoted value stop writing its home word.
    pub(in crate::ir_lower) deopt_named_reachable: Vec<bool>,
    /// Carried single-use values whose home write this compile removed. See
    /// [`Lowerer::extend_home_drops_to_carried_values`].
    pub(in crate::ir_lower) carried_homes_dropped: usize,
    /// Per-block CSE of the mapped-receiver guard (null, alignment, the six
    /// read-bounds compares): receivers already proven in the block being
    /// lowered. Cleared at every block entry (`ir_receiver_guard_cse_enabled`).
    pub(in crate::ir_lower) guarded_receivers: Vec<NodeId>,
    /// Inline field-read sites emitted so far in the block being lowered.
    ///
    /// Census only — nothing branches on it. Every site past the first emits a
    /// layout-epoch guard and a per-object compactness test that the site
    /// before it already executed, and neither has a CSE the way the receiver
    /// null test does. This counter is what says how large that population is
    /// on a given workload, which is the question to answer before building
    /// the CSE: the elision is not free to write, because each guard branches
    /// to ITS OWN slow path and then rejoins, so a later site cannot simply
    /// assume an earlier one passed.
    pub(in crate::ir_lower) block_inline_field_sites: usize,
    /// Round 16 (lane fieldguard, FG16-1, `CRATONVM_JIT_IR_LAYOUT_GUARD_DEOPT`):
    /// `(receiver, block)` pairs whose compact layout a DEOPTING layout-epoch
    /// guard has proven, in emission order. Never cleared within a compile;
    /// see [`Lowerer::emit_field_layout_guard`] for which later sites may read
    /// a pair.
    pub(in crate::ir_lower) layout_proven: Vec<(NodeId, usize)>,
    /// `CRATONVM_JIT_IR_LAYOUT_GUARD_DEOPT`, read once per compile.
    pub(in crate::ir_lower) layout_guard_deopt_on: bool,
    /// The switch is on and the block CFG has no cycle: a proof may then be
    /// read in any block its guard's block dominates (no OSR entry can enter
    /// past a dominator, and no node runs twice per activation).
    pub(in crate::ir_lower) layout_guard_cross_block: bool,
    /// Bytecode pcs exactly ONE node outside `op_cannot_deopt` carries (empty
    /// with the switch off): the sites whose deopt re-executes nothing but
    /// themselves. See [`layout_guard_lone_pcs`].
    pub(in crate::ir_lower) layout_guard_lone_pcs: std::collections::HashSet<usize>,
    /// JIT round 17 wave 3 (lane irlower; item 3 of
    /// `r17-monitor2-by-bci-readers-that-stay-by-bci-FIXED-20261005.md`): under
    /// `CRATONVM_JIT_IR_LAYOUT_GUARD_LONE_PER_FRAME` (opt-in, and only with the
    /// deopting guard's own switch on), the lone sites counted per
    /// `(pc, own frame)` instead of per pc ([`layout_guard_lone_sites`]).
    /// `None` keeps [`Self::layout_guard_lone_pcs`]' rule.
    pub(in crate::ir_lower) layout_guard_lone_sites:
        Option<std::collections::HashSet<(usize, Option<u32>)>>,
    /// JIT round 25 (item 4, `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS`), read
    /// once per compile. See [`Lowerer::proven_receiver_mode`].
    pub(in crate::ir_lower) proven_fields_on: bool,
    /// Item 4: `(receiver, block)` pairs whose `GC_FLAG_COMPACT` bit a
    /// DEOPTING compactness guard has proven, in emission order -- the twin
    /// of [`Self::layout_proven`], read under the same dominance rule
    /// ([`Lowerer::field_proof_holds`]). Never cleared within a compile: the
    /// bit is a property of the object, fixed at its allocation.
    pub(in crate::ir_lower) compact_proven: Vec<(NodeId, usize)>,
    /// Item 4: per-body census of the inline field accesses, printed under
    /// `CRATONVM_DBG_JITC` at the end of the compile.
    pub(in crate::ir_lower) proven_field_census: ProvenFieldCensus,
    /// Item 4 wave 2 (`CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS_SPLICED`, with
    /// the main switch): a field site inside a spliced body may trap when a
    /// chain snapshot resumes it at its own bci ([`Lowerer::field_site_chain_trap_ok`]).
    pub(in crate::ir_lower) proven_spliced_on: bool,
    /// Item 4 wave 2 (`CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS_DOMINANCE`, with
    /// the main switch): field proofs are read across blocks by dominance in
    /// every body, under the entry and identity rules of
    /// [`Lowerer::field_proof_reaches`].
    pub(in crate::ir_lower) proven_scope_on: bool,
    /// Item 4 residual 2 (`CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS_PREHEADER`,
    /// with the dominance switch): a proof from before a loop covers its
    /// body, the OSR header entries re-checking it
    /// ([`Lowerer::field_proof_reaches`]).
    pub(in crate::ir_lower) proven_preheader_on: bool,
    /// Item 4 residual 4 (`CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS_CATCH_ARMS`,
    /// with the dominance switch): compiled handler arms no longer refuse
    /// every cross-block read ([`Lowerer::field_proof_reaches`], rule 4).
    pub(in crate::ir_lower) proven_catch_arms_on: bool,
    /// Wave 2: blocks that lie on a CFG cycle, computed on first use.
    pub(in crate::ir_lower) field_proof_cycle_blocks: std::cell::OnceCell<Vec<bool>>,
    /// Wave 2: every block control can enter other than through its CFG
    /// predecessors from the method entry -- OSR entry candidates (loop
    /// headers, opaque OSR arms) and call-site exception pad targets --
    /// computed on first use. A superset of the real entries.
    pub(in crate::ir_lower) field_proof_entries: std::cell::OnceCell<Vec<usize>>,
    /// Wave 2: `(guard block, use block)` -> the entries that reach the use
    /// without entering the guard block: `None` when one of them is an entry
    /// no stub can re-check (an opaque OSR arm, an exception pad), else the
    /// ordinary OSR entries (residual 2: each must re-check the proof).
    /// Memoised [`Lowerer::field_proof_reaches`].
    pub(in crate::ir_lower) field_proof_scope_cache:
        std::cell::RefCell<HashMap<(usize, usize), Option<Vec<usize>>>>,
    /// Item 4 residual 2 (pre-header proofs): `(entry block, receiver,
    /// compact)` -- an ordinary OSR entry at that block jumps past the guard
    /// of a proof a site read ([`Lowerer::field_proof_reaches`]), so its stub
    /// must re-check it ([`Lowerer::osr_entry_field_proof_checks`]): the
    /// receiver's compactness when `compact`, else the layout epoch. An entry
    /// that cannot is refused.
    pub(in crate::ir_lower) field_proof_osr_obligations:
        std::cell::RefCell<Vec<(usize, NodeId, bool)>>,
    /// Item 4 residual 2: memoised [`Lowerer::field_proof_canon`].
    pub(in crate::ir_lower) field_proof_canon_cache: std::cell::RefCell<HashMap<NodeId, NodeId>>,
    /// W6-E (`CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS_COESTABLISH`, default ON since j27,
    /// with the main switch): an establishing `getfield` on another value
    /// also proves the method's receiver ([`Lowerer::coestablish_receiver_proof`]).
    pub(in crate::ir_lower) proven_coestablish_on: bool,
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
    pub(in crate::ir_lower) null_proven_receivers: Vec<NodeId>,
    /// JIT r23 wave 7 (lane L8, applied by L1; `CRATONVM_JIT_IR_UNBOX_GUARD_CSE`):
    /// `(receiver, class id)` pairs whose `Op::Unbox` null test and
    /// exact-class guard already passed in the block being lowered. Both
    /// guards deopt on failure and never fall through, an object's class never
    /// changes and a node's value is fixed, so a later `Op::Unbox` on the same
    /// node and class in the same block needs neither. Block-local like
    /// `null_proven_receivers` (cleared in `lower_block`). The layout-epoch
    /// compare and the compact-bit select stay per read: the epoch can move at
    /// a safepoint.
    pub(in crate::ir_lower) unbox_class_proven: Vec<(NodeId, u32)>,
    pub(in crate::ir_lower) checkcast_null_or_exact_proven: Vec<(NodeId, u32)>,
    pub(in crate::ir_lower) unsigned_byte_array_loads: Vec<bool>,
    pub(in crate::ir_lower) elided_byte_mask_ands: HashMap<NodeId, NodeId>,
    pub(in crate::ir_lower) fused_index_and: Vec<Option<(NodeId, u32)>>,
    pub(in crate::ir_lower) extra_dead_add_remats: HashMap<NodeId, Vec<(NodeId, i32, bool)>>,
    pub(in crate::ir_lower) extra_pad_remats: HashMap<usize, Vec<PadRemat>>,
    pub(in crate::ir_lower) block_cse: std::cell::Cell<crate::ir_lower::block_cse::BlockCseState>,
    /// nio21 wave 5, lane L4 (`CRATONVM_JIT_IR_SCOPED_SCREEN_CSE`): the base
    /// nodes whose scoped heap arm already ran its one-dimensional
    /// primitive-array screen in the block being lowered
    /// ([`Lowerer::emit_scoped_heap_array_address`]). A later heap arm on the
    /// same node skips the screen's deopt branch: an object's array kind never
    /// changes, and the earlier screen dominates every later node of its block
    /// (a null base takes neither heap arm). Cleared at every block entry.
    pub(in crate::ir_lower) scoped_screened_bases: Vec<NodeId>,
    /// Register → memory transitions this backend EMITTED for resident values
    /// (one per resident definition, because the wiring is write-through).
    /// Reported as `CompilationReport::spills`.
    pub(in crate::ir_lower) ls_spills: usize,
    /// Memory → register transitions emitted for resident values: the
    /// materialisation of a value whose definition arm computes into RAX
    /// (`Op::ConstF`, `Op::Neg`, `Op::Param`) and therefore reaches its
    /// register through its home word. Reported as
    /// `CompilationReport::reloads`.
    pub(in crate::ir_lower) ls_reloads: usize,

    // ── Level 2: the machine list ────────────────────────────────────
    //
    // Empty and `MirMode::Off` unless `CRATONVM_JIT=ir-isel-emit` or
    // `ir-isel-verify` is set. See [`MirPlan`].
    /// The selector's output for this method, one entry per scheduled block.
    pub(in crate::ir_lower) mir: Option<MirPlan>,
    /// The selected operands and omitted definitions used by both planners.
    pub(in crate::ir_lower) value_flow: crate::regalloc::ValueFlow,
    /// What to do with it.
    pub(in crate::ir_lower) mir_mode: MirMode,
    /// Tiles whose bytes this compile took from the level-2 encoder
    /// (`MirMode::Emit`) or checked against the per-opcode arms
    /// (`MirMode::Verify`).
    pub(in crate::ir_lower) mir_tiles: usize,
    /// Byte disagreements between the two paths. Non-zero refuses the compile:
    /// increment 2 is where fail-closed returns.
    pub(in crate::ir_lower) mir_mismatches: usize,
    /// Verify mode only, and it decides whether there is a next increment:
    /// tiles the encoder can express but is NOT allowed to emit, because their
    /// bytes are not identical to the per-opcode arm's.
    pub(in crate::ir_lower) mir_shadow_tiles: usize,
    /// Bytes the per-opcode arms wrote for those tiles' nodes…
    pub(in crate::ir_lower) mir_arm_bytes: usize,
    /// …and bytes the level-2 encoder would have written instead.
    pub(in crate::ir_lower) mir_enc_bytes: usize,

    // ── Call-site exception edges (round nio21 wave 3, lane L2) ──────
    //
    // All empty / zero / false unless the graph carries `ir::Op::CallThrew`
    // (`CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES`, default OFF) and
    // `plan_call_throw_edges` armed a site.
    // (`local_handler_lookup` is shared with the round-19 compiled handler
    // entry above: one field.)
    /// Call node -> its armed plan.
    pub(in crate::ir_lower) call_throw_plans: HashMap<NodeId, CallThrowPlan>,
    /// Blocks an armed pad jumps into (each the true edge of an
    /// `If(CallThrew)`); `lower_block` enters them with no register-cache
    /// state carried over.
    pub(in crate::ir_lower) call_throw_entry_blocks: std::collections::HashSet<usize>,
    /// JIT round 21 wave 22 (lane cf, `CRATONVM_JIT_IR_FRESH_VOLATILE_STORE_UNFENCED`):
    /// the volatile `Op::Store`s whose trailing StoreLoad fence is elided
    /// ([`fresh_allocation_unfenced_volatile_stores`]). `None` until the first
    /// volatile store of the compile asks; an empty set with the switch off.
    pub(in crate::ir_lower) unfenced_fresh_stores: Option<std::collections::HashSet<NodeId>>,
    /// JIT round 23 close (`CRATONVM_JIT_IR_VOLATILE_FENCE_SINK`): per block,
    /// may a volatile store's StoreLoad fence still be owed on entry? `None`
    /// until the first block asks ([`Self::ensure_fence_sink_plan`]); an empty
    /// vector with the switch off, so every store fences where it stands.
    pub(in crate::ir_lower) fence_sink_in: Option<Vec<bool>>,
    /// The fence owed at the current emission point under the sink.
    pub(in crate::ir_lower) fence_pending: bool,
    /// Some store of this compile defers its fence: every poll slow path then
    /// pays one, so no safepoint exit leaves owing it.
    pub(in crate::ir_lower) fence_sink_deferred_any: bool,
    /// The plan of the `Op::Call` being lowered, if armed.
    pub(in crate::ir_lower) cur_call_throw: Option<CallThrowPlan>,
    /// The ordinary call has reached the publication boundary. Prefix exits
    /// before it retain their own exception route.
    pub(in crate::ir_lower) call_throw_publication: Option<CallThrowPublication>,
    /// nio21 wave 4 L2: the exit being recorded is a native-stack guard's
    /// (`push_stack_guard_exc_patch`): a `StackOverflowError`, which no catch
    /// type a spliced site admits can take, so it keeps today's exit even for
    /// a spliced site.
    pub(in crate::ir_lower) call_throw_stack_guard_exit: bool,
    /// JIT round 25 wave 6 (W6-D): the leaf-entry plan of this attempt and
    /// what its verification counts ([`LeafEntryState`]).
    pub(in crate::ir_lower) leaf_entry: LeafEntryState,
    /// JIT round 25 wave 7 (W7-C): the tail-call republish sink of this
    /// attempt ([`TailRepublishSink`]).
    pub(in crate::ir_lower) tail_sink: TailRepublishSink,
    /// The pads, emitted after the body.
    pub(in crate::ir_lower) call_throw_pads: Vec<CallThrowPad>,
    /// The sites the pads bake, moved into the artifact.
    pub(in crate::ir_lower) call_throw_sites: Vec<Box<crate::JitLocalHandlerSite>>,
    /// Pads emitted. Diagnostic (`CRATONVM_DBG_JITC`).
    pub(in crate::ir_lower) call_throw_pads_emitted: usize,

    // ── Implicit exception edges (round nio21 wave 5, lane L2) ───────
    //
    // Empty / zero unless the graph carries an `ir::Op::CallThrew` whose
    // anchor is a trapping node rather than a call
    // (`CRATONVM_JIT_IR_IMPLICIT_EXCEPTION_EDGES`, default ON, `=0` off).
    /// Trapping node (`ArrayLoad` / `ArrayStore` / the division's `Guard`) ->
    /// its armed plan ([`Self::plan_implicit_throw_site`]).
    pub(in crate::ir_lower) implicit_throw_plans: HashMap<NodeId, ImplicitThrowPlan>,
    /// `JitRuntimeHelpers::throw_aioobe`, `throw_arithmetic` and
    /// `jit_npe_with_action`: the single-pass stubs' signal setters, which
    /// an implicit pad calls before the lookup.
    pub(in crate::ir_lower) throw_aioobe: usize,
    pub(in crate::ir_lower) throw_arithmetic: usize,
    pub(in crate::ir_lower) npe_with_action: usize,
    /// Implicit pads emitted (one per failing check of an armed site).
    /// Diagnostic (`CRATONVM_DBG_JITC`).
    pub(in crate::ir_lower) implicit_throw_pads_emitted: usize,
}

/// One trapping node armed with implicit exception edges (round nio21 wave
/// 5, lane L2; `ir::ir_implicit_exception_edges_enabled`), planned before
/// emission by [`Lowerer::plan_implicit_throw_site`].
#[derive(Clone, Debug)]
pub(in crate::ir_lower) struct ImplicitThrowPlan {
    /// The `JitLocalHandlerSite` the pad hands `jit_local_handler_lookup`.
    pub(in crate::ir_lower) site_ptr: u64,
    /// `targets[k]` = the block of the true edge of `If(CallThrew { k })`.
    pub(in crate::ir_lower) targets: Vec<usize>,
    /// The anchor's own `Ref` home, nulled on the taken path (`aaload`: the
    /// load never ran, and a later map in the handler names the word), as
    /// `CallThrowPad::result_home` does for a call.
    pub(in crate::ir_lower) result_home: Option<i32>,
    /// Catch `(class_name, handler_bci)` pairs in table order, used to fold
    /// a statically-matched, dead-exception local catch into a direct branch.
    pub(in crate::ir_lower) candidates: Vec<(&'static str, u32)>,
}

/// Which implicit exception an implicit pad raises (nio21 wave 5 L2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum ImplicitKind {
    /// A null array, or (nio21 wave 6) a null `getfield` / `putfield`
    /// receiver: `jit_npe_with_action` with the access's JEP 358 code
    /// (`NONE` for a field access; the message is rebuilt from the bytecode).
    Npe,
    /// An index out of bounds: `jit_throw_aioobe(index, length, 0, bci)`,
    /// entered with the array in RAX and the index in ECX.
    Aioobe,
    /// A zero divisor: `jit_set_throw_bci(bci)` + `jit_throw_arithmetic()`.
    Arithmetic,
}

/// `CRATONVM_JIT_IR_DEFER_TRAP_SNAPSHOT_TO_PROBE` (JIT round 20 wave 18, lane
/// xThrow, THROW20-6 of `docs/feature-designs/jit-ir-optimizations.md`,
/// proposal L5W17-1), **default ON**, `=0` off; also off when the single-pass
/// tier's `CRATONVM_JIT_DEFER_TRAP_SNAPSHOT_TO_PROBE=0`. The bit
/// ([`crate::implicit_null::TRAP_SNAPSHOT_DEFER_BIT`]) an armed implicit pad
/// ORs into the bci it hands its setter (`jit_set_throw_bci` before
/// `jit_npe_with_action` / `jit_throw_arithmetic`, `jit_throw_aioobe`'s fourth
/// argument), else `0`.
///
/// With the bit the setter records a `DeferredTrapSnapshot` instead of walking
/// the compiled frames; [`Lowerer::emit_implicit_throw_pad`]'s
/// `jit_local_handler_lookup`, which ALWAYS follows the setter in that pad,
/// takes the snapshot (or, for a fast throw, does not) while the same frames
/// stand. The IR pad does not clear its safepoint-id slot between the two
/// calls (the single-pass probe does), so the walk places this frame at `bci`
/// through the map the pad published, and the trap description (NPE key,
/// stamped bci) is applied on top as the setter applied it: the same snapshot.
/// Read per armed pad at compile time, no static.
pub(in crate::ir_lower) fn ir_defer_trap_snapshot_bit() -> u64 {
    use cratonvm_types::flags::runtime_flag_default_on;
    {
        // Cast: a positive bit below 2^31.
        crate::implicit_null::TRAP_SNAPSHOT_DEFER_BIT as u64
    }
}

/// What the level-2 machine list is for on this compile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum MirMode {
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
pub(in crate::ir_lower) struct MirPlan {
    /// One selection per scheduled block, in block order.
    pub(in crate::ir_lower) blocks: Vec<crate::x64::isel::BlockSelection>,
    /// `tile_of[node]` = index into `blocks[b].tiles` of the tile rooted at
    /// `node`, for the block `b` the node was scheduled into.
    ///
    /// A node a tile *absorbed* is deliberately absent: it has no tile of its
    /// own, and the encoder must never be asked to emit one for it.
    pub(in crate::ir_lower) tile_of: Vec<Option<(u32, u32)>>,
}
