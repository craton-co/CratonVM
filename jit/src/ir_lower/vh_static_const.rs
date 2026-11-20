// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 2 (lane VHCOMP; page
//! `docs/feature-designs/native-dispatch-architecture.md`):
//! a constant STATIC-field `VarHandle` access (`VH.getAndAdd(1)` through a
//! `static final VarHandle` from `findStaticVarHandle`) compiled as the
//! hardware access on the static itself -- `LOCK XADD` / `LOCK CMPXCHG` /
//! `XCHG` / `MOV` on the static's payload word -- instead of the ~1.6 us call
//! into the `VarHandle` natives (HotSpot 25: 12 ns).
//!
//! # What the VM publishes
//!
//! One [`VhStaticCell`] per `static final VarHandle` field `(class, index)`,
//! owned by the VM for its whole life (`vm/src/jit/vh_static_cells.rs`) and
//! filed by the MUTATOR: the natives' static arms
//! (`native-builtins` `lang_invoke::member_name::vh_static_slot_of`) report
//! every handle they resolve for an initialized holder, and the VM fills the
//! cell of each `static final` that holds exactly that handle object -- with
//! the target static's statics base cell, `field_index * 16` and its declared
//! kind (`I` / `J`). Only for a read-write, non-exact handle, and only while
//! the natives' static atomics are `hw_static_cell_update`'s one hardware CAS
//! on the same payload word (`CRATONVM_HW_STATIC_CAS`, the statics index): the
//! STATICCAS21-2 argument (`direct_helpers::unsafe_inline_static_cell_bytes`),
//! unchanged. A class unload clears the cells that name the class.
//!
//! # What compiled code does
//!
//! The compile asks only for the cell's ADDRESS
//! ([`WELL_KNOWN_VH_CONST_STATIC_CELL_TAG`]); the cell is read at run time, so
//! a site compiled before the natives filed it starts hitting as soon as they
//! do, and every refusal is a decline to the unchanged call:
//!
//! ```text
//!   MOV RCX, cell ; MOV RAX, [RCX] ; TEST RAX, RAX ; JZ call   ; not filed
//!   CMP DWORD [RCX + 12], kind ; JNE call                        ; site kind
//!   MOV R11D, [RCX + 8]                                           ; index * 16
//!   MOV RAX, [RAX] ; TEST RAX, RAX ; JZ call                      ; the block
//!   CMP DWORD [RAX + R11], tag ; JNE call                         ; Int / Long cell
//!   ADD R11, payload
//!   <the access at [RAX + R11]>                                   ; result in RAX
//! ```
//!
//! The holder's initialization guard is the filing rule: a cell is filled
//! only after the natives answered for an INITIALIZED holder (their
//! `<clinit>` paths run before), so an access the cell serves is one whose
//! holder is initialized -- HotSpot's `getstatic` barrier, decided once.
//! Barriers: none owed (a primitive payload; statics are roots). No GC point,
//! no call, no deopt on the hit path.

use super::{
    enc_frame_store, vh_const_ask, vh_const_inline_enabled, vh_const_query,
    vh_const_site_shape, FrameAccess, IrType, JitInvokeInfo, Lowerer, NodeId, Op, VhConstOp,
    VhConstShape, R10, RDX,
};
use crate::direct_helpers::{
    unsafe_inline_rmw_bytes, UnsafeInlineRmw, UNSAFE_STATIC_TAG_INT, UNSAFE_STATIC_TAG_LONG,
};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// NOT a class id: a [`vh_const_query`]-shaped word asking for the address of
/// the [`VhStaticCell`] of the `static final VarHandle` field `(class,
/// index)`, `0` when the VM keeps none (not a candidate, switch off).
pub const WELL_KNOWN_VH_CONST_STATIC_CELL_TAG: i64 = 0x5690_i64 << 48;

/// `CRATONVM_JIT_VH_CONST_STATIC` (j27 wave 2, lane VHCOMP; perf, **default
/// ON**, `=0` keeps every static-field constant-`VarHandle` site on its call
/// and files no cell): see the module comment. Also off under
/// `CRATONVM_JIT_VH_CONST_INLINE=0`. Read per site at compile time and by the
/// VM when it files, so no cache.
pub fn vh_const_static_enabled() -> bool {
    vh_const_inline_enabled()
}

/// Byte offset of [`VhStaticCell`]'s `disp` word.
pub const VH_STATIC_CELL_DISP_OFFSET: u8 = 8;
/// Byte offset of [`VhStaticCell`]'s `kind` word.
pub const VH_STATIC_CELL_KIND_OFFSET: u8 = 12;

/// What compiled code reads for one `static final VarHandle` field. Layout
/// pinned by [`VH_STATIC_CELL_DISP_OFFSET`] / [`VH_STATIC_CELL_KIND_OFFSET`].
#[repr(C)]
#[derive(Debug, Default)]
pub struct VhStaticCell {
    /// The target static's statics base cell (`StaticsIndex::base_cell_addr`),
    /// `0` while not filed (or after an unload cleared it).
    base_cell: AtomicUsize,
    /// `field_index * SLOT_SIZE`: the static's cell in its statics block.
    disp: AtomicU32,
    /// The static's declared kind, `I` or `J`.
    kind: AtomicU32,
}

const _: () = assert!(std::mem::offset_of!(VhStaticCell, disp) == VH_STATIC_CELL_DISP_OFFSET as usize);
const _: () = assert!(std::mem::offset_of!(VhStaticCell, kind) == VH_STATIC_CELL_KIND_OFFSET as usize);

impl VhStaticCell {
    /// An unfilled cell.
    pub const fn new() -> Self {
        Self {
            base_cell: AtomicUsize::new(0),
            disp: AtomicU32::new(0),
            kind: AtomicU32::new(0),
        }
    }

    /// Point the cell at static `field_index` of the block behind
    /// `base_cell`, of declared `kind`. `false` (nothing written) for a kind
    /// other than `I` / `J`, a null base cell, or an index whose displacement
    /// does not fit 32 bits. The base cell is written last, with release, so
    /// a reader that sees it sees the other two words.
    pub fn fill(&self, base_cell: usize, field_index: usize, kind: u8) -> bool {
        let Some(disp) = field_index
            .checked_mul(cratonvm_types::SLOT_SIZE)
            .and_then(|d| u32::try_from(d).ok())
        else {
            return false;
        };
        if base_cell == 0 || !matches!(kind, b'I' | b'J') {
            return false;
        }
        self.kind.store(u32::from(kind), Ordering::Relaxed);
        self.disp.store(disp, Ordering::Relaxed);
        self.base_cell.store(base_cell, Ordering::Release);
        true
    }

    /// Unfile the cell: every compiled site reading it declines from now on.
    pub fn clear(&self) {
        self.base_cell.store(0, Ordering::Release);
    }

    /// `(base cell, disp, kind)` when filled.
    pub fn filed(&self) -> Option<(usize, u32, u8)> {
        let base = self.base_cell.load(Ordering::Acquire);
        // Cast: `fill` stores a `u8` kind.
        (base != 0).then(|| {
            (
                base,
                self.disp.load(Ordering::Relaxed),
                self.kind.load(Ordering::Relaxed) as u8,
            )
        })
    }
}

/// A static-field `VarHandle` access-mode call the static prefix may serve:
/// [`vh_const_site_shape`]'s rules for the same name and descriptor WITHOUT a
/// receiver coordinate (`getAndAdd(I)I`, `compareAndSet(JJ)Z`, `get()I`,
/// `set(J)V`, a discarded RMW's `(I)V`, ...), `int` and `long` only: a
/// reference static would owe the collector's SATB pre-barrier.
pub(crate) fn vh_static_site_shape(method: &str, descriptor: &str) -> Option<VhConstShape> {
    let rest = descriptor.strip_prefix('(')?;
    let shape = vh_const_site_shape(method, &format!("(LA;{rest}"))?;
    matches!(shape.kind, b'I' | b'J').then_some(shape)
}

/// The address of the VM's [`VhStaticCell`] for the `static final` field
/// `(class_id, field_index)`, or `None` (switch off, no cell, no VM).
pub(crate) fn vh_static_cell_addr(
    helpers: &crate::DirectHelperTable,
    class_id: u32,
    field_index: usize,
) -> Option<usize> {
    if class_id == 0 || !vh_const_static_enabled() {
        return None;
    }
    let field_index = u16::try_from(field_index).ok()?;
    let addr = vh_const_ask(
        helpers,
        vh_const_query(WELL_KNOWN_VH_CONST_STATIC_CELL_TAG, class_id, field_index),
    );
    usize::try_from(addr)
        .ok()
        .filter(|&a| a != 0 && a % std::mem::align_of::<VhStaticCell>() == 0)
}

/// The run-time screen of the module comment, for a site of `kind`. On a hit
/// RAX = the statics block and R11 = the payload's displacement in it. Writes
/// RAX, RCX, R11 and the flags only (RDX and R10 keep the operands). Returns
/// the bytes and the rel32 decline fields.
pub(crate) fn vh_static_screen_bytes(cell: usize, kind: u8) -> (Vec<u8>, Vec<usize>) {
    let wide = kind == b'J';
    let mut b: Vec<u8> = Vec::with_capacity(64);
    let mut declines: Vec<usize> = Vec::new();
    let jcc = |b: &mut Vec<u8>, cc: u8| -> usize {
        b.extend_from_slice(&[0x0F, cc, 0, 0, 0, 0]);
        b.len() - 4
    };
    b.extend_from_slice(&[0x48, 0xB9]); // MOV RCX, imm64
    b.extend_from_slice(&(cell as u64).to_le_bytes()); // Cast: an address
    b.extend_from_slice(&[0x48, 0x8B, 0x01]); // MOV RAX, [RCX]: the base cell
    b.extend_from_slice(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
    declines.push(jcc(&mut b, 0x84)); // JZ: not filed
    b.extend_from_slice(&[0x83, 0x79, VH_STATIC_CELL_KIND_OFFSET, kind]); // CMP DWORD [RCX+12], kind
    declines.push(jcc(&mut b, 0x85)); // JNE: another kind
    b.extend_from_slice(&[0x44, 0x8B, 0x59, VH_STATIC_CELL_DISP_OFFSET]); // MOV R11D, [RCX+8]
    b.extend_from_slice(&[0x48, 0x8B, 0x00]); // MOV RAX, [RAX]: the statics block
    b.extend_from_slice(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
    declines.push(jcc(&mut b, 0x84)); // JZ: no block
    let tag = if wide {
        UNSAFE_STATIC_TAG_LONG
    } else {
        UNSAFE_STATIC_TAG_INT
    };
    b.extend_from_slice(&[0x42, 0x83, 0x3C, 0x18, tag]); // CMP DWORD [RAX + R11], tag
    declines.push(jcc(&mut b, 0x85)); // JNE: not an Int / Long cell
    let payload = if wide {
        cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET
    } else {
        cratonvm_types::FIELD_CELL_PAYLOAD32_OFFSET
    };
    // Cast: 4 or 8.
    b.extend_from_slice(&[0x49, 0x83, 0xC3, payload as u8]); // ADD R11, imm8
    (b, declines)
}

/// The access at `[RAX + R11]` (after [`vh_static_screen_bytes`]) with RDX =
/// the value / delta / new value and R10 = `expected`; RAX = the site's result
/// after (an `int` sign-extended, a CAS `0` / `1`). Gets are plain loads and
/// `set` / `setOpaque` / `setRelease` plain stores (x86-TSO gives acquire and
/// release); `setVolatile` is `XCHG`; the RMW modes are the `Unsafe`
/// prefixes' shared bytes, which are fully fenced.
pub(crate) fn vh_static_access_bytes(op: VhConstOp, kind: u8) -> Vec<u8> {
    let wide = kind == b'J';
    let rex = if wide { 0x4A } else { 0x42 };
    match op {
        VhConstOp::Get => {
            let mut b = vec![rex, 0x8B, 0x04, 0x18]; // MOV EAX / RAX, [RAX + R11]
            if !wide {
                b.extend_from_slice(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
            }
            b
        }
        // MOV / XCHG [RAX + R11], EDX / RDX.
        VhConstOp::Set { volatile } => vec![rex, if volatile { 0x87 } else { 0x89 }, 0x14, 0x18],
        VhConstOp::Rmw(op) => unsafe_inline_rmw_bytes(UnsafeInlineRmw {
            width: if wide { 8 } else { 4 },
            op,
        }),
    }
}

/// The IR node type a site of `shape` must have.
fn vh_static_node_type(shape: &VhConstShape) -> IrType {
    match shape.ret {
        b'V' => IrType::Void,
        b'J' => IrType::Long,
        _ => IrType::Int,
    }
}

impl Lowerer<'_> {
    /// The static arm of `try_emit_ir_vh_const_prefix` (see the module
    /// comment): `true` when it emitted the guarded prefix (and set
    /// `call_prefix_join`), `false` with nothing emitted. The contract of the
    /// instance arm: scratch RAX, RCX, RDX, R10, R11 only; the hit edge writes
    /// the result home and register and joins the call's continuation.
    pub(super) fn try_emit_ir_vh_const_static_prefix(
        &mut self,
        id: NodeId,
        info: &JitInvokeInfo,
    ) -> bool {
        let Some(shape) = vh_static_site_shape(info.method_name, info.descriptor) else {
            return false;
        };
        if !vh_const_static_enabled() {
            return false;
        }
        let Some((cell, operands)) = self.vh_static_site(id, info, &shape) else {
            return false;
        };
        let slot = if shape.ret == b'V' {
            None
        } else {
            match self.vh_static_result_slot(id) {
                Ok(slot) => slot,
                Err(why) => return self.vh_static_declined(id, info, why),
            }
        };
        self.emit_vh_static_prefix(id, &shape, cell, operands, slot);
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] vh-const-static (ir) n{id} {}{} kind={} cell=0x{cell:x}",
                info.method_name,
                info.descriptor,
                char::from(shape.kind),
            );
        }
        true
    }

    /// The site's cell and `(expected, value)` operands, or `None` (declined,
    /// named under `CRATONVM_DBG_JITC`).
    fn vh_static_site(
        &self,
        id: NodeId,
        info: &JitInvokeInfo,
        shape: &VhConstShape,
    ) -> Option<(usize, (Option<NodeId>, Option<NodeId>))> {
        let graph = self.graph;
        let node = graph.nodes.get(id as usize)?;
        // [ctrl, mem, handle, values...]
        if node.inputs.len() != 3 + shape.values.len() || node.ty != vh_static_node_type(shape) {
            self.vh_static_declined(id, info, "operand count or node type");
            return None;
        }
        if node.inputs[2..].iter().any(|&i| self.carry_names(i)) {
            self.vh_static_declined(id, info, "an operand is held by a carry");
            return None;
        }
        let Some(Op::LoadStatic {
            class_id,
            field_index,
            ..
        }) = graph.nodes.get(node.inputs[2] as usize).map(|n| &n.op)
        else {
            self.vh_static_declined(id, info, "handle operand is not a getstatic");
            return None;
        };
        // Cast: a u32 static index.
        let Some(cell) =
            vh_static_cell_addr(&self.direct_helpers, *class_id, *field_index as usize)
        else {
            self.vh_static_declined(id, info, "no static cell for the handle's field");
            return None;
        };
        let operands = match shape.values.len() {
            2 => (Some(node.inputs[3]), Some(node.inputs[4])),
            1 => (None, Some(node.inputs[3])),
            _ => (None, None),
        };
        Some((cell, operands))
    }

    /// The planned result destination, admitting dropped homes with a resident GPR.
    fn vh_static_result_slot(&self, id: NodeId) -> Result<Option<i32>, &'static str> {
        let want_ty = self
            .graph
            .nodes
            .get(id as usize)
            .map_or(IrType::Void, |n| n.ty);
        self.prefix_result_slot(id, want_ty)
            .ok_or("result is carried, routed, or has no destination")
    }

    /// Emit the prefix: operands first (the screen writes neither RDX nor
    /// R10), the screen, the access, the result, the hit edge.
    fn emit_vh_static_prefix(
        &mut self,
        id: NodeId,
        shape: &VhConstShape,
        cell: usize,
        (expected, value): (Option<NodeId>, Option<NodeId>),
        slot: Option<i32>,
    ) {
        self.note_rcx_written();
        if let Some(expected) = expected {
            self.gp_load_value(R10, expected);
        }
        if let Some(value) = value {
            self.gp_load_value(RDX, value);
        }
        let (screen, edges) = vh_static_screen_bytes(cell, shape.kind);
        let at = self.buf.pos();
        self.buf.emit(&screen);
        let declines: Vec<usize> = edges.into_iter().map(|e| at + e).collect();
        self.buf.emit(&vh_static_access_bytes(shape.op, shape.kind));
        if shape.ret != b'V' {
            self.emit_prefix_result_store(id, slot);
        }
        let hit = self.emit_jmp_rel32();
        for p in declines {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// Name a compile-time decline under `CRATONVM_DBG_JITC`; `false`.
    fn vh_static_declined(&self, id: NodeId, info: &JitInvokeInfo, why: &str) -> bool {
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] vh-const-static (ir) n{id} DECLINED {}{}: {why}",
                info.method_name, info.descriptor
            );
        }
        false    }}

#[cfg(test)]
mod j27_vhcomp_static_shape_tests;

/// The screen and the access, executed against a fake statics block.
#[cfg(all(test, target_arch = "x86_64"))]
mod j27_vhcomp_static_exec_tests;
