// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 4 (lane CLONEIR; item 2 of
//! `docs/known-issues/jit/j27-clone-spliced-instance-sites-and-inline-expansion-proposal-20261028.md`,
//! `CRATONVM_JIT_IR_CLONE_INLINE`, opt-in): an ARRAY `clone()` expanded
//! inline, as HotSpot's C2 does (`LibraryCallKit::inline_native_clone`: the
//! allocation plus an `ArrayCopy`), as a guarded PREFIX of the site's
//! `Op::Call`.
//!
//! # Which sites
//!
//! Only a VIRTUAL `X.clone()Ljava/lang/Object;` whose static class `X` is an
//! array (`[I`, `[[J`, `[Ljava/lang/String;`, ...) and which this compile
//! bound to a thin clone helper (`object_clone_virtual`, or
//! `byte_array_clone` for `[B`). So it only ever replaces the helper's SERVED
//! array arm (`array_clone_serve`): `clone()` of an array is always
//! `Object.clone`, always `Cloneable`, never overridden, never finalizable,
//! and never throws `CloneNotSupportedException`. An instance receiver never
//! reaches this prefix (its class name does not start with `[`).
//!
//! # The hit path (no call, no safepoint, nothing that can collect)
//!
//! 1. The receiver screen: non-null, and its kind-tags byte is exactly the
//!    site's (one primitive element type, or a reference array); no JVMTI
//!    `VMObjectAlloc` / `SampledObjectAlloc` listener
//!    ([`crate::direct_helpers::JVMTI_ALLOC_EVENT_BITS`] in the VM's event
//!    word, as `clone_vm_object_alloc_armed` asks); its length and class word
//!    parked in the phi-copy scratch word (two `int`s, never a reference);
//!    for a reference array the SATB pre gate idle (no marking window).
//! 2. The TLAB bump of a compiled `newarray` / `anewarray` with the
//!    receiver's class word and length
//!    ([`crate::runtime_lowering::emit_inline_tlab_array_runtime_class_fast_path_ir`]):
//!    `0` for every primitive array, the component's id for a reference
//!    array -- the copy `Object.clone`'s native allocates, header word for
//!    header word, with a fresh mark (no hash, no lock).
//! 3. For a reference array: the copy is young by the published store plan,
//!    or no old object exists (the gated `aastore`'s post gate). Then a raw
//!    word copy owes the collector nothing: a fresh object needs no pre
//!    barrier, and a young one no card.
//! 4. The payload, in whole aligned 8-byte words up to the rounded data size
//!    (both data areas are rounded to 8 and the copy's is fresh, so no word
//!    is torn and no element is split across two accesses).
//! 5. The result home (and register), then the join.
//!
//! Every miss DECLINES to the unchanged call (the thin helper, which owns the
//! NPE, every collector's barriered copy and the large-array ladder) with
//! nothing committed -- except the reference post gate (3), which runs on
//! the committed copy and declines leaving a zeroed, unreferenced array in
//! the TLAB, as the helper's own copy refusal does (`array_clone_serve`).
//!
//! # Collectors
//!
//! Primitive arrays: Generational, G1 and ZGC (the inline bump's own announce
//! rules). Reference arrays: Generational only (a published store plan, no
//! registration, 8-byte slots, no armed ZGC read barrier); G1 and ZGC keep
//! the helper, which carries their barriers.

use super::{
    enc_frame_store, ir_inline_tlab_enabled, ir_inline_tlab_newarray_enabled, FrameAccess,
    IrType, JitInvokeInfo, Lowerer, NodeId, RAX, RCX,
};
use crate::direct_helpers::JVMTI_ALLOC_EVENT_BITS;
use crate::runtime_lowering::{
    emit_inline_tlab_array_runtime_class_fast_path_ir, patch_rel32_to_here, InlineTlabArrayPlan,
};
use crate::ExecutableBuffer;
use cratonvm_types::ArrayElementType;

const CLONE_DESCRIPTOR: &str = "()Ljava/lang/Object;";

const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: x86-64 disp8
const LENGTH: u8 = cratonvm_types::ARRAY_LENGTH_OFFSET as u8; // Cast: x86-64 disp8
const DATA: u8 = cratonvm_types::ARRAY_DATA_OFFSET as u8; // Cast: x86-64 disp8
const _: () = assert!(
    cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127
        && cratonvm_types::ARRAY_LENGTH_OFFSET <= 127
        && cratonvm_types::ARRAY_DATA_OFFSET <= 127
        && cratonvm_types::ARRAY_DATA_OFFSET % 8 == 0,
    "the inline clone bakes disp8 header offsets and an 8-aligned payload"
);

/// `CRATONVM_JIT_IR_CLONE_INLINE` (j27 wave 4, lane CLONEIR; perf, **opt-in**:
/// unmeasured and unexecuted in a VM at the time of writing). Read per site
/// at compile time; no static.
pub(crate) fn ir_clone_inline_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_CLONE_INLINE")
}

/// What an array `clone()` site's static class says the copy is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ArrayCloneShape {
    /// The JVM atype (`T_BOOLEAN` 4 .. `T_LONG` 11), `0` for a reference array.
    atype: u8,
    /// The receiver's whole kind-tags byte (kind in bits 0..1, element type
    /// in 2..5, 6..7 reserved zero on arrays).
    kind_tag: u8,
    /// log2 of the element size in bytes.
    log2_elem: u8,
}

impl ArrayCloneShape {
    fn of(elem: ArrayElementType) -> Option<Self> {
        let log2_elem = match cratonvm_types::element_byte_size(elem) {
            1 => 0,
            2 => 1,
            4 => 2,
            8 => 3,
            _ => return None,
        };
        let atype = elem as u8; // Cast: the atype discriminant
        let kind = cratonvm_types::ObjectKind::Array as u8; // Cast: a 2-bit kind
        Some(Self {
            atype,
            kind_tag: kind | (atype << 2),
            log2_elem,
        })
    }

    fn is_reference(self) -> bool {
        self.atype == ArrayElementType::Reference as u8 // Cast: the atype discriminant
    }
}

/// The site screen: the copy's shape when the call is a virtual
/// `clone()Ljava/lang/Object;` of an array class and the switch is on.
pub(super) fn array_clone_site_shape(
    class: &str,
    name: &str,
    desc: &str,
    invoke_kind: u8,
) -> Option<ArrayCloneShape> {
    if invoke_kind != 0 || name != "clone" || desc != CLONE_DESCRIPTOR || !ir_clone_inline_enabled()
    {
        return None;
    }
    ArrayCloneShape::of(array_element_of(class)?)
}

/// The element type of the array class `class`, or `None` for a non-array.
fn array_element_of(class: &str) -> Option<ArrayElementType> {
    if let Some(tag) = cratonvm_types::primitive_array_kind_tags_byte(class) {
        return cratonvm_types::array_element_type_from_tag(tag >> 2);
    }
    (class.starts_with("[L") || class.starts_with("[[")).then_some(ArrayElementType::Reference)
}

/// How the reference post gate decides that the copy is young.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum YoungTest {
    /// `TEST flags, mask; JZ young` (`Lowerer::ref_store_post_skip_mask`).
    Mask(u8),
    /// `CMP flags, byte [floor]; JB young`.
    Floor(usize),
}

/// The published store-plan gates a reference payload copy needs
/// (`Lowerer::gated_ir_aastore_gates`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RefCopyGates {
    pub pre: usize,
    pub post: usize,
    pub young: YoungTest,
}

/// Everything [`emit_array_clone`] bakes.
pub(super) struct ArrayClonePlan {
    pub shape: ArrayCloneShape,
    /// The bump. Its `length_off` is the scratch word: the length in its low
    /// dword, the receiver's class word in its high dword, which is the
    /// emitter's class-word frame offset `length_off - 4`.
    pub bump: InlineTlabArrayPlan,
    pub stub_target: usize,
    /// The receiver's frame home (re-read after the bump, which clobbers
    /// every scratch register).
    pub receiver_off: i32,
    /// [`crate::DirectHelperTable::jvmti_alloc_events_word`] (`0`: no test).
    pub jvmti_word: usize,
    /// `Some` exactly for a reference array.
    pub ref_gates: Option<RefCopyGates>,
}

/// The inline clone. Entry: RAX = the receiver, also in `[RBP -
/// receiver_off]`. Falls through with RAX = the copy; every decline edge is
/// appended to `decline`. Returns `false` when the bump refused: the bytes
/// emitted so far wrote only the scratch word, and the caller lands every
/// edge, and the fall-through, on the call. Scratch: RAX, RCX, RDX, R8-R11
/// and the flags.
pub(super) fn emit_array_clone(
    buf: &mut ExecutableBuffer,
    plan: &ArrayClonePlan,
    decline: &mut Vec<usize>,
) -> bool {
    if plan.shape.is_reference() != plan.ref_gates.is_some()
        || plan.receiver_off <= 0
        || plan.bump.length_off <= 4
        || plan.bump.atype != plan.shape.atype
    {
        return false;
    }
    emit_receiver_screen(buf, plan, decline);
    let class_word_off = plan.bump.length_off - 4;
    if !emit_inline_tlab_array_runtime_class_fast_path_ir(
        buf,
        &plan.bump,
        plan.stub_target,
        class_word_off,
        decline,
    ) {
        return false;
    }
    if let Some(gates) = plan.ref_gates {
        emit_young_copy_gate(buf, gates, decline);
    }
    emit_payload_copy(buf, plan.shape.log2_elem, plan.receiver_off);
    true
}

/// Step 1 of the module comment. Entry RAX = the receiver; RAX is kept.
fn emit_receiver_screen(
    buf: &mut ExecutableBuffer,
    plan: &ArrayClonePlan,
    decline: &mut Vec<usize>,
) {
    buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
    decline.push(jcc32(buf, 0x84)); // JZ: the call raises the NPE
    buf.emit(&[0x80, 0x78, KIND_TAGS, plan.shape.kind_tag]); // CMP BYTE [RAX+tags], tag
    decline.push(jcc32(buf, 0x85)); // JNE: another element type
    if plan.jvmti_word != 0 {
        buf.emit(&jvmti_alloc_event_test_bytes(plan.jvmti_word));
        decline.push(jcc32(buf, 0x85)); // JNZ: an allocation listener is armed
    }
    buf.emit(&[0x8B, 0x48, LENGTH]); // MOV ECX, [RAX+len] (zero-extends)
    buf.emit(&[0x8B, 0x10]); // MOV EDX, [RAX] (the class word)
    buf.emit(&[0x48, 0xC1, 0xE2, 0x20]); // SHL RDX, 32
    buf.emit(&[0x48, 0x09, 0xD1]); // OR RCX, RDX
    let mut park = FrameAccess::new();
    enc_frame_store(RCX, plan.bump.length_off, &mut park);
    buf.emit(park.as_slice()); // MOV [RBP - scratch], RCX
    if let Some(gates) = plan.ref_gates {
        mov_r11_imm64(buf, gates.pre as u64); // Cast: a data address
        buf.emit(&[0x41, 0x80, 0x3B, 0x00]); // CMP byte [R11], 0
        decline.push(jcc32(buf, 0x85)); // JNE: a marking window
    }
}

/// Step 3 of the module comment. Entry RAX = the committed copy; RAX is kept.
fn emit_young_copy_gate(
    buf: &mut ExecutableBuffer,
    gates: RefCopyGates,
    decline: &mut Vec<usize>,
) {
    buf.emit(&[0x0F, 0xB6, 0x88]); // MOVZX ECX, byte [RAX + disp32]
    // Cast: a header byte offset, far below i32::MAX.
    buf.emit(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());
    let young = match gates.young {
        YoungTest::Mask(mask) => {
            buf.emit(&[0xF6, 0xC1, mask]); // TEST CL, mask
            jcc32(buf, 0x84) // JZ: young
        }
        YoungTest::Floor(floor) => {
            mov_r11_imm64(buf, floor as u64); // Cast: a data address
            buf.emit(&[0x41, 0x3A, 0x0B]); // CMP CL, byte [R11]
            jcc32(buf, 0x82) // JB: young
        }
    };
    mov_r11_imm64(buf, gates.post as u64); // Cast: a data address
    buf.emit(&[0x41, 0x80, 0x3B, 0x00]); // CMP byte [R11], 0
    decline.push(jcc32(buf, 0x85)); // JNE: old objects exist
    patch_rel32_to_here(buf, young);
}

/// Step 4 of the module comment. Entry RAX = the copy; RAX is kept.
fn emit_payload_copy(buf: &mut ExecutableBuffer, log2_elem: u8, receiver_off: i32) {
    buf.emit(&[0x44, 0x8B, 0x58, LENGTH]); // MOV R11D, [RAX+len]
    if log2_elem != 0 {
        buf.emit(&[0x49, 0xC1, 0xE3, log2_elem]); // SHL R11, log2
    }
    buf.emit(&[0x49, 0x83, 0xC3, 0x07]); // ADD R11, 7
    buf.emit(&[0x49, 0xC1, 0xEB, 0x03]); // SHR R11, 3: whole words
    buf.emit(&[0x4C, 0x8B, 0x95]); // MOV R10, [RBP + disp32]
    buf.emit(&(-receiver_off).to_le_bytes());
    buf.emit(&[0x4D, 0x8D, 0x52, DATA]); // LEA R10, [R10+data]
    buf.emit(&[0x48, 0x8D, 0x48, DATA]); // LEA RCX, [RAX+data]
    buf.emit(&CLONE_WORD_COPY_LOOP);
}

/// The word copy of [`emit_payload_copy`]. Entry: R10 = source word address,
/// RCX = destination word address, R11 = word count >= 0. Clobbers RDX, RCX,
/// R10, R11 and the flags; RAX is kept. Forward (the copy is fresh, so the
/// two never overlap).
///
/// ```text
///  0: TEST R11, R11       ; 4D 85 DB
///  3: JZ   done (24)      ; 74 13
///  5: MOV  RDX, [R10]     ; 49 8B 12      top
///  8: MOV  [RCX], RDX     ; 48 89 11
/// 11: ADD  R10, 8         ; 49 83 C2 08
/// 15: ADD  RCX, 8         ; 48 83 C1 08
/// 19: DEC  R11            ; 49 FF CB
/// 22: JNZ  top (5)        ; 75 ED
/// 24:                                      done
/// ```
const CLONE_WORD_COPY_LOOP: [u8; 24] = [
    0x4D, 0x85, 0xDB, // TEST R11, R11
    0x74, 0x13, // JZ done
    0x49, 0x8B, 0x12, // MOV RDX, [R10]
    0x48, 0x89, 0x11, // MOV [RCX], RDX
    0x49, 0x83, 0xC2, 0x08, // ADD R10, 8
    0x48, 0x83, 0xC1, 0x08, // ADD RCX, 8
    0x49, 0xFF, 0xCB, // DEC R11
    0x75, 0xED, // JNZ top
];

/// The JVMTI allocation-event test of every inline allocation that stands in
/// front of a helper declining to a posting native (this module's clone, the
/// `Array.newArray` prefix's `reflect_new_array_resolve_bytes`): `MOV R11,
/// word; TEST DWORD [R11], JVMTI_ALLOC_EVENT_BITS`. The caller emits the
/// `JNZ decline` and calls this only for a non-zero
/// [`crate::DirectHelperTable::jvmti_alloc_events_word`]. Writes R11 and the
/// flags.
pub(super) fn jvmti_alloc_event_test_bytes(word: usize) -> [u8; 17] {
    let mut bytes = [0u8; 17];
    bytes[..2].copy_from_slice(&[0x49, 0xBB]); // MOV R11, imm64
    bytes[2..10].copy_from_slice(&(word as u64).to_le_bytes()); // Cast: a data address
    bytes[10..13].copy_from_slice(&[0x41, 0xF7, 0x03]); // TEST DWORD [R11], imm32
    bytes[13..].copy_from_slice(&JVMTI_ALLOC_EVENT_BITS.to_le_bytes());
    bytes
}

fn jcc32(buf: &mut ExecutableBuffer, cc: u8) -> usize {
    buf.emit(&[0x0F, cc]);
    let patch = buf.pos();
    buf.emit(&[0; 4]);
    patch
}

fn mov_r11_imm64(buf: &mut ExecutableBuffer, value: u64) {
    buf.emit(&[0x49, 0xBB]); // MOV R11, imm64
    buf.emit(&value.to_le_bytes());
}

impl Lowerer<'_> {
    /// The inline array `clone()` prefix of the `Op::Call` `id` (see the
    /// module comment). Emits nothing unless the site screen, the helper bind
    /// and every compile-time admission agree.
    pub(super) fn try_emit_ir_array_clone_prefix(&mut self, id: NodeId, info_ptr: usize) {
        if info_ptr == 0 {
            return;
        }
        // SAFETY: as every other read of an `Op::Call`'s `info_ptr` in this
        // file's parent -- a live `JitInvokeInfo` owned by `ir_call_infos`.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        let Some(shape) = array_clone_site_shape(
            info.class_name,
            info.method_name,
            info.descriptor,
            info.invoke_kind,
        ) else {
            return;
        };
        match self.array_clone_plan(id, shape) {
            Ok((plan, slot)) => self.emit_array_clone_prefix(id, info, &plan, slot),
            Err(why) => self.note_string_prefix_declined(id, info, why),
        }
    }

    /// The plan and the result's frame slot, or why the site keeps its call.
    fn array_clone_plan(
        &mut self,
        id: NodeId,
        shape: ArrayCloneShape,
    ) -> Result<(ArrayClonePlan, i32), &'static str> {
        let receiver = self.array_clone_receiver(id)?;
        let ref_gates = self.array_clone_ref_gates(shape)?;
        let (stub_target, announce) = self.array_clone_bump_target(shape)?;
        let scratch = self.phi_copy_scratch_slot_off;
        if scratch <= 4 {
            return Err("no scratch word");
        }
        let receiver_off = self
            .slot_of_checked(receiver)
            .map_err(|_| "a receiver without a frame home")?;
        let slot = self.planned_slot_off(id).map_err(|_| "no result slot")?;
        let bump = InlineTlabArrayPlan {
            thread_slot_off: self.shadow_thread_slot_off,
            thread_tls_disp: self.ir_tlab_thread_tls_disp(),
            cursor_off: self.tlab_cursor_off,
            end_off: self.tlab_end_off,
            // Named by the plan, unread on its stub-free path; the `Op::Call`
            // row of `scan_frame_needs` reserves the slot anyway.
            context_off: self.context_slot_off,
            atype: shape.atype,
            length_off: scratch,
            announce,
            // The class word is the run-time one; `Some` only admits the
            // reference arm.
            component_class_id: shape.is_reference().then_some(0),
            const_length: None,
        };
        let plan = ArrayClonePlan {
            shape,
            bump,
            stub_target,
            receiver_off,
            jvmti_word: self.direct_helpers.jvmti_alloc_events_word,
            ref_gates,
        };
        Ok((plan, slot))
    }

    /// The receiver of a call bound to a thin clone helper, with a frame
    /// home, and a result that has one too.
    fn array_clone_receiver(&self, id: NodeId) -> Result<NodeId, &'static str> {
        let graph = self.graph;
        let node = graph.node_opt(id).ok_or("no node")?;
        // [ctrl, mem, receiver].
        if node.inputs.len() != 3 || node.ty != IrType::Ref {
            return Err("node shape");
        }
        let helpers = [
            self.direct_helpers.object_clone_virtual,
            self.direct_helpers.byte_array_clone,
        ];
        let bound = node
            .bytecode_pc
            .and_then(|pc| self.direct_calls.get(&pc))
            .is_some_and(|&(entry, _)| entry != 0 && helpers.contains(&entry));
        if !bound {
            return Err("not bound to a thin clone helper");
        }
        if !crate::ir_thin_prefix_enabled() || crate::x64::zgc_read_barrier_blocks_inline_fields()
        {
            return Err("thin prefixes off / armed ZGC read barrier");
        }
        let receiver = node.inputs[2];
        let dropped = |n: NodeId| self.home_dropped.get(n as usize).copied().unwrap_or(false);
        if self.carry_names(receiver)
            || dropped(receiver)
            || self.carry_of.get(id as usize).copied().flatten().is_some()
            || dropped(id)
        {
            return Err("a carried or home-less operand");
        }
        Ok(receiver)
    }

    /// The store-plan gates of a reference payload copy (`None` for a
    /// primitive one), or why this collector keeps the helper.
    fn array_clone_ref_gates(
        &self,
        shape: ArrayCloneShape,
    ) -> Result<Option<RefCopyGates>, &'static str> {
        if !shape.is_reference() {
            return Ok(None);
        }
        if cratonvm_types::jit_tlab_registration_required()
            || cratonvm_types::zgc_read_barrier_armed()
            || cratonvm_types::element_byte_size(ArrayElementType::Reference) != 8
        {
            return Err("a reference copy on ZGC or narrow slots");
        }
        let (pre, post, floor) = self
            .gated_ir_aastore_gates()
            .ok_or("no published reference-store plan")?;
        let young = match self.ref_store_post_skip_mask {
            Some(mask) => YoungTest::Mask(mask),
            None => YoungTest::Floor(floor),
        };
        Ok(Some(RefCopyGates { pre, post, young }))
    }

    /// The bump's stub target (tested for non-zero only) and announce helper,
    /// or why the inline bump is not admitted.
    fn array_clone_bump_target(
        &self,
        shape: ArrayCloneShape,
    ) -> Result<(usize, usize), &'static str> {
        let registration = cratonvm_types::jit_tlab_registration_required();
        let bump = ir_inline_tlab_enabled()
            && if registration {
                crate::x64::zgc_inline_array_announce_enabled() && self.zgc_note_tlab_object != 0
            } else {
                ir_inline_tlab_newarray_enabled()
            };
        let reference_bump = crate::runtime_lowering::ir_inline_anewarray_any_gc_enabled()
            && crate::x64::inline_tlab_anewarray_enabled();
        if !bump || (shape.is_reference() && !reference_bump) {
            return Err("inline array bump not admitted");
        }
        let target = if shape.is_reference() {
            self.anewarray_object
        } else {
            self.newarray
        };
        if target == 0 {
            return Err("no allocation helper");
        }
        let announce = if registration {
            self.zgc_note_tlab_object
        } else {
            0
        };
        Ok((target, announce))
    }

    /// Emit the planned prefix: the clone, the result home, the join.
    fn emit_array_clone_prefix(
        &mut self,
        id: NodeId,
        info: &JitInvokeInfo,
        plan: &ArrayClonePlan,
        slot: i32,
    ) {
        if crate::ir_stage_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] clone-inline (ir) n{id} class={} ref={} jvmti={}",
                info.class_name,
                plan.shape.is_reference(),
                plan.jvmti_word != 0
            );
        }
        self.note_rcx_written();
        let receiver = self
            .graph
            .node_opt(id)
            .and_then(|n| n.inputs.get(2).copied());
        let Some(receiver) = receiver else {
            return;
        };
        self.gp_load_value(RAX, receiver);
        let mut decline: Vec<usize> = Vec::new();
        if !emit_array_clone(&mut self.buf, plan, &mut decline) {
            self.note_string_prefix_declined(id, info, "the bump emitter refused");
            for p in decline {
                self.patch_rel32_to_here(p);
            }
            return;
        }
        if let Some(reg) = self.assigned_gpr(id) {
            self.emit_mov_reg_reg64(reg, RAX);
        }
        let mut home = FrameAccess::new();
        enc_frame_store(RAX, slot, &mut home);
        self.buf.emit(home.as_slice());
        crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod j27_clone_inline_tests {
    use super::*;

    const KEY: &str = "CRATONVM_JIT_IR_CLONE_INLINE";
    const UNTOUCHED: u64 = 0xA5A5_A5A5_A5A5_A5A5;
    /// Frame words: thread [RBP-8], context [RBP-16], receiver [RBP-24], the
    /// scratch word [RBP-40] (class word at [RBP-36]).
    const THREAD_OFF: i32 = 8;
    const CONTEXT_OFF: i32 = 16;
    const RECEIVER_OFF: i32 = 24;
    const SCRATCH_OFF: i32 = 40;
    const TLAB_WORDS: usize = 64;

    fn shape(class: &str) -> Option<ArrayCloneShape> {
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("1"))], || {
            array_clone_site_shape(class, "clone", CLONE_DESCRIPTOR, 0)
        })
    }

    /// Every primitive array class, `int[][]` and `String[]` have a shape
    /// whose tag is the header's; anything else has none.
    #[test]
    fn j27_the_site_screen_names_every_array_and_nothing_else() {
        for class in ["[Z", "[B", "[C", "[S", "[I", "[F", "[J", "[D"] {
            let s = shape(class).expect(class);
            assert_eq!(Some(s.kind_tag), cratonvm_types::primitive_array_kind_tags_byte(class));
            let elem = cratonvm_types::array_element_type_from_tag(s.atype).expect(class);
            assert_eq!(1usize << s.log2_elem, cratonvm_types::element_byte_size(elem), "{class}");
            assert!(!s.is_reference(), "{class}");
        }
        for class in ["[[I", "[Ljava/lang/String;", "[[Ljava/lang/Object;"] {
            let s = shape(class).expect(class);
            assert!(s.is_reference(), "{class}");
            assert_eq!(s.kind_tag, cratonvm_types::ObjectKind::Array as u8);
        }
        assert_eq!(shape("java/util/ArrayList"), None);
        assert_eq!(shape("[Q"), None);
        let with = |name: &str, desc: &str, kind: u8| {
            cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("1"))], || {
                array_clone_site_shape("[I", name, desc, kind)
            })
        };
        assert_eq!(with("clone", CLONE_DESCRIPTOR, 1), None, "invokespecial");
        assert_eq!(with("hashCode", "()I", 0), None);
        let off = cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            array_clone_site_shape("[I", "clone", CLONE_DESCRIPTOR, 0)
        });
        assert_eq!(off, None, "opt-in");
    }

    fn plan(
        shape: ArrayCloneShape,
        jvmti_word: usize,
        ref_gates: Option<RefCopyGates>,
    ) -> ArrayClonePlan {
        ArrayClonePlan {
            shape,
            bump: InlineTlabArrayPlan {
                thread_slot_off: THREAD_OFF,
                thread_tls_disp: 0,
                cursor_off: 0,
                end_off: 8,
                context_off: CONTEXT_OFF,
                atype: shape.atype,
                length_off: SCRATCH_OFF,
                announce: 0,
                component_class_id: shape.is_reference().then_some(0),
                const_length: None,
            },
            stub_target: 0x1000,
            receiver_off: RECEIVER_OFF,
            jvmti_word,
            ref_gates,
        }
    }

    /// `extern "C" fn(thread, receiver) -> copy | 0`.
    fn build(plan: &ArrayClonePlan) -> Option<ExecutableBuffer> {
        let mut buf = ExecutableBuffer::new(2048)?;
        // PUSH RBP; MOV RBP, RSP; SUB RSP, 64.
        buf.emit(&[0x55, 0x48, 0x89, 0xE5, 0x48, 0x83, 0xEC, 0x40]);
        let args = super::super::ENTRY_ABI_REGS;
        for (off, reg) in [(THREAD_OFF, args[0]), (RECEIVER_OFF, args[1])] {
            let mut store = FrameAccess::new();
            enc_frame_store(reg, off, &mut store);
            buf.emit(store.as_slice());
        }
        buf.emit(&[0x48, 0x8B, 0x45, 0xE8]); // MOV RAX, [RBP-24]
        let mut decline = Vec::new();
        if !emit_array_clone(&mut buf, plan, &mut decline) {
            return None;
        }
        buf.emit(&[0xC9, 0xC3]); // LEAVE; RET -- RAX is the copy
        for p in decline {
            patch_rel32_to_here(&mut buf, p);
        }
        buf.emit(&[0x31, 0xC0, 0xC9, 0xC3]); // XOR EAX, EAX; LEAVE; RET
        assert!(!buf.overflowed());
        buf.finalize();
        Some(buf)
    }

    /// A receiver array: header (`class`, `elem`, `length`), then `payload`.
    fn receiver(class: u32, elem: ArrayElementType, length: u32, payload: &[u64]) -> Vec<u64> {
        let header = cratonvm_types::ObjectHeader::new(
            cratonvm_types::ClassId::new(class),
            cratonvm_types::ObjectKind::Array,
            elem,
            length,
            length,
        );
        let mut words = vec![header.header_word(), u64::from(length)];
        words.extend_from_slice(payload);
        words
    }

    fn address(words: &[u64]) -> u64 {
        words.as_ptr() as u64 // Cast: a test object's address
    }

    /// One clone from a fresh TLAB with `room` bytes: `(copy offset or None,
    /// cursor advance, TLAB words)`.
    fn run(buf: &ExecutableBuffer, room: u64, recv: u64) -> (Option<u64>, u64, Vec<u64>) {
        let mut tlab = vec![UNTOUCHED; TLAB_WORDS];
        let base = tlab.as_mut_ptr() as u64; // Cast: the TLAB's address
        let mut thread = [base, base + room, 0, 0, 0, 0];
        // SAFETY: `buf` is a finalized `extern "C" fn(u64, u64) -> u64` that
        // reads the receiver words and the thread block and writes at most
        // `room` bytes of `tlab`; it calls nothing.
        let f = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(buf.as_ptr())
        };
        let copy = f(thread.as_mut_ptr() as u64, recv); // Cast: the thread block
        let at = (copy != 0).then_some(copy.wrapping_sub(base));
        (at, thread[0].wrapping_sub(base), tlab)
    }

    fn bump_switches_off() -> bool {
        !crate::x64::inline_tlab_newarray_enabled()
            || !crate::x64::inline_tlab_new_enabled()
            || !crate::x64::inline_tlab_zero_elision_enabled()
    }

    /// Every primitive element type: the copy is the allocator's header for
    /// the receiver's class word and length, its payload the receiver's,
    /// word for word up to the rounded data size, nothing past it written.
    #[test]
    fn j27_every_primitive_array_clones_exactly() {
        for class in ["[Z", "[B", "[C", "[S", "[I", "[F", "[J", "[D"] {
            let s = shape(class).expect(class);
            let Some(buf) = build(&plan(s, 0, None)) else {
                assert!(bump_switches_off(), "{class}: declined with every switch on");
                return;
            };
            let elem = cratonvm_types::array_element_type_from_tag(s.atype).expect(class);
            for length in [0u32, 1, 3, 7, 9] {
                // Cast: a small test length.
                let words = ((length as usize) << s.log2_elem).saturating_add(7) / 8;
                let payload: Vec<u64> = (0x1111_0000u64..).take(words).collect();
                let recv = receiver(0, elem, length, &payload);
                let (at, used, tlab) = run(&buf, 512, address(&recv));
                assert_eq!(at, Some(0), "{class} length={length}");
                assert_eq!(used, 16 + 8 * words as u64, "{class} footprint"); // Cast: small
                let fresh = cratonvm_types::ObjectHeader::new(
                    cratonvm_types::ClassId::new(0),
                    cratonvm_types::ObjectKind::Array,
                    elem,
                    length,
                    length,
                );
                assert_eq!(tlab[0], fresh.header_word(), "{class} header");
                assert_eq!(tlab[1] as u32, length, "{class} length"); // Cast: the length dword
                assert_eq!(&tlab[2..2 + words], &payload[..], "{class} payload");
                let past = &tlab[2 + words..];
                assert!(past.iter().all(|&w| w == UNTOUCHED), "{class}: past the copy");
            }
        }
    }

    /// Null, another element type, an armed allocation listener and a full
    /// TLAB decline with the TLAB untouched; an unrelated armed event serves.
    #[test]
    fn j27_every_decline_leaves_the_tlab_untouched() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let s = shape("[I").expect("[I");
        let events = Box::new(AtomicU32::new(0));
        let word = &*events as *const AtomicU32 as usize; // Cast: the event word's address
        let Some(buf) = build(&plan(s, word, None)) else {
            assert!(bump_switches_off());
            return;
        };
        let ints = receiver(0, ArrayElementType::Int, 2, &[0x0000_0002_0000_0001]);
        let longs = receiver(0, ArrayElementType::Long, 1, &[7]);
        let cases: [(u32, u64, u64, &str); 5] = [
            (0, 512, 0, "null"),
            (0, 512, address(&longs), "long[] at an int[] site"),
            (4, 512, address(&ints), "VMObjectAlloc"),
            (8, 512, address(&ints), "SampledObjectAlloc"),
            (0, 16, address(&ints), "a full TLAB"),
        ];
        for (armed, room, recv, why) in cases {
            events.store(armed, Ordering::Relaxed);
            let (at, used, tlab) = run(&buf, room, recv);
            assert_eq!(at, None, "{why}");
            assert_eq!(used, 0, "{why}: the cursor must not move");
            assert!(tlab.iter().all(|&w| w == UNTOUCHED), "{why}");
        }
        events.store(1, Ordering::Relaxed); // FieldWatch: not an allocation event
        let (at, _, tlab) = run(&buf, 512, address(&ints));
        assert_eq!(at, Some(0));
        assert_eq!(tlab[2], 0x0000_0002_0000_0001);
        drop(events);
    }

    fn ref_gates(pre: &u8, post: &u8, young: YoungTest) -> RefCopyGates {
        RefCopyGates {
            pre: pre as *const u8 as usize, // Cast: a gate byte's address
            post: post as *const u8 as usize, // Cast: a gate byte's address
            young,
        }
    }

    /// `int[][]` / `String[]` shape: the copy keeps the receiver's component
    /// class word and copies every reference; a marking window declines
    /// uncommitted; an old copy with old objects present declines after the
    /// commit (the orphan the helper also leaves), body unwritten.
    #[test]
    fn j27_a_reference_array_copies_under_the_gates() {
        if cratonvm_types::element_byte_size(ArrayElementType::Reference) != 8 {
            return;
        }
        const COMPONENT: u32 = 0x4321;
        let s = shape("[[I").expect("[[I");
        let (idle, busy) = (Box::new(0u8), Box::new(1u8));
        // `post` busy throughout: old objects exist, so only youth admits.
        let gated = |pre: &u8, young: YoungTest| plan(s, 0, Some(ref_gates(pre, &busy, young)));
        let floor_at = |byte: &u8| YoungTest::Floor(byte as *const u8 as usize); // Cast: address
        let Some(buf) = build(&gated(&*idle, YoungTest::Mask(0))) else {
            assert!(
                bump_switches_off()
                    || !crate::runtime_lowering::ir_inline_anewarray_any_gc_enabled()
                    || !crate::x64::inline_tlab_anewarray_enabled()
            );
            return;
        };
        let elems = [0x7000_1000u64, 0, 0x7000_2000];
        let recv = receiver(COMPONENT, ArrayElementType::Reference, 3, &elems);
        let (at, used, tlab) = run(&buf, 512, address(&recv));
        assert_eq!(at, Some(0));
        assert_eq!(used, 16 + 24);
        let fresh = cratonvm_types::ObjectHeader::new(
            cratonvm_types::ClassId::new(COMPONENT),
            cratonvm_types::ObjectKind::Array,
            ArrayElementType::Reference,
            3,
            3,
        );
        assert_eq!(tlab[0], fresh.header_word(), "the component class word");
        assert_eq!(&tlab[2..5], &elems[..]);
        assert!(tlab[5..].iter().all(|&w| w == UNTOUCHED));
        // A marking window: declined before the bump.
        let buf = build(&gated(&*busy, YoungTest::Mask(0))).expect("built once");
        let (at, used, tlab) = run(&buf, 512, address(&recv));
        assert_eq!((at, used), (None, 0));
        assert!(tlab.iter().all(|&w| w == UNTOUCHED));
        // Young by the floor (flags below 0xFF): served.
        let high = Box::new(0xFFu8);
        let buf = build(&gated(&*idle, floor_at(&*high))).expect("built once");
        assert_eq!(run(&buf, 512, address(&recv)).0, Some(0));
        // Old by the floor (nothing is below 0), old objects present: the
        // committed orphan, its body not copied.
        let buf = build(&gated(&*idle, floor_at(&*idle))).expect("built once");
        let (at, used, tlab) = run(&buf, 512, address(&recv));
        assert_eq!(at, None);
        assert_eq!(used, 16 + 24, "the bump was committed");
        assert!(tlab[2..5].iter().all(|&w| w == UNTOUCHED), "the body is not copied");
        drop((idle, busy, high));
    }

    /// The emitter refuses, emitting nothing, a plan whose gates disagree
    /// with its shape or that has no scratch word for the class word.
    #[test]
    fn j27_an_inconsistent_plan_emits_nothing() {
        let mut buf = ExecutableBuffer::new(256).expect("buffer");
        let mut decline = Vec::new();
        let ints = shape("[I").expect("[I");
        let refs = shape("[Ljava/lang/String;").expect("refs");
        let byte = 0u8;
        let gates = ref_gates(&byte, &byte, YoungTest::Mask(0));
        assert!(!emit_array_clone(&mut buf, &plan(ints, 0, Some(gates)), &mut decline));
        assert!(!emit_array_clone(&mut buf, &plan(refs, 0, None), &mut decline));
        let mut no_scratch = plan(ints, 0, None);
        no_scratch.bump.length_off = 4;
        assert!(!emit_array_clone(&mut buf, &no_scratch, &mut decline));
        assert_eq!(buf.pos(), 0);
        assert!(decline.is_empty());
    }
}
