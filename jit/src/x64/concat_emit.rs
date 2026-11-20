// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The machine code of an inline `StringConcatFactory` concatenation that
//! BOTH tiers emit, over a bare [`ExecutableBuffer`] (JIT round 19 proposal
//! wave 5, lane concat5).
//!
//! Wave 4 (lane strgap4, STRGAP19-3, `CRATONVM_JIT_CONCAT_INLINE`) built the
//! inline concat as `Compiler` methods of the single-pass backend
//! (`op_concat.rs`). The optimizing tier's twin (STRGAP4-1,
//! `CRATONVM_JIT_IR_CONCAT_INLINE`, `ir_lower`'s `Op::IndyBridge` arm) has no
//! `Compiler`, so the parts that do not depend on one live here and both tiers
//! call them:
//!
//! * [`emit_length_pass`] / [`emit_fill_pass`]: the two passes over the
//!   operands, which each tier stages in the same place -- one word per
//!   operand in a frame block (`[rbp - off]`), the bridge call's argument
//!   buffer -- so the passes read frame words and nothing else;
//! * the pure renderers ([`emit_decimal`], [`emit_digit_count`],
//!   [`emit_literal`], [`emit_copy`]);
//! * [`emit_ir_tail`]: the optimizing tier's allocation (one TLAB bump for
//!   the `byte[]` and the `String`). The single-pass backend keeps its own
//!   tail, the shared STR16-P1 `emit_sb_to_string_tail`, whose thread source
//!   (the single-pass cached slot, else the TLS mirror) and counted declines
//!   the IR tier does not have. The two tails make the same stores in the same order, and the
//!   collector pre-checks here are byte-for-byte the uncounted
//!   `Compiler::emit_sb_g1_same_region_check` /
//!   `emit_zgc_start_bit_precheck` / `emit_zgc_start_bit_store_prechecked`
//!   (pinned by the tests at the bottom), so the two tiers cannot drift on
//!   what a collector is promised.
//!
//! Every decline precedes the first store, and the fast path makes no call,
//! polls nothing and runs no Java code: a declined evaluation repeats no side
//! effect and lands on the unchanged bridge call with every staged operand
//! where the bridge reads it.

use super::op_invoke::{SbToStringGc, SbToStringPlan};
use crate::concat_inline_plan::{ConcatArgKind, ConcatSegment, JitConcatInlinePlan};
use crate::ExecutableBuffer;

/// `(x * CONCAT_DIV10_MAGIC) >> 67 == x / 10` for every unsigned 64-bit `x`.
const CONCAT_DIV10_MAGIC: u64 = 0xCCCC_CCCC_CCCC_CCCD;

// The registers this module names (the backend's own numbering).
const RAX: u8 = 0;
const RCX: u8 = 1;
const RDX: u8 = 2;
const R8: u8 = 8;
const R9: u8 = 9;
const R10: u8 = 10;
const R11: u8 = 11;

/// The compile-time facts both passes read, per site.
#[derive(Clone, Debug)]
pub(crate) struct ConcatPassShape {
    pub(crate) plan: &'static JitConcatInlinePlan,
    pub(crate) outer: crate::StringFieldLayout,
    /// This VM's `compact_strings` flag byte.
    pub(crate) compact_flag_addr: usize,
    /// `[rbp - off]` of each operand's staged word, by operand index.
    pub(crate) arg_offsets: Vec<i32>,
    /// `helpers.class_id_offset_in_obj`.
    pub(crate) class_id_off: i32,
    /// `ARRAY_LENGTH_OFFSET` / `ARRAY_DATA_OFFSET` as disp8 bytes.
    pub(crate) len_d8: u8,
    pub(crate) data_d8: u8,
    /// The largest result served inline (`SbToStringPlan::max_count`).
    pub(crate) max_count: i32,
    /// CONCAT21-4 (`CRATONVM_JIT_CONCAT_VALUE_OF_FUSION`, single-pass only):
    /// a `String` operand may hold an exact `java/lang/Integer` that a fused
    /// `String.valueOf(o)` passed through unconverted; the passes render its
    /// `int` (`Integer.toString`). `None`: every non-`String` declines, as
    /// before.
    pub(crate) boxed_int: Option<ConcatBoxedInt>,
    /// CONCAT21-6: resolved `(class_id, compact_off, legacy_off)` for
    /// descriptor-typed boxed wrappers (`Integer`, `Long`, `Character`, `Boolean`).
    pub(crate) boxed_layouts: ConcatBoxedLayouts,
}

/// CONCAT21-4: how the passes read an exact `Integer` operand's `value`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConcatBoxedInt {
    /// `java/lang/Integer`'s class id.
    pub(crate) class_id: u32,
    /// `value`'s 4-byte payload in a COMPACT / LEGACY instance.
    pub(crate) compact_off: i32,
    pub(crate) legacy_off: i32,
}

/// CONCAT21-6: resolved `(class_id, compact_off, legacy_off)` for each
/// descriptor-typed boxed wrapper used in a plan.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ConcatBoxedLayouts {
    pub(crate) int: Option<ConcatBoxedInt>,
    pub(crate) long: Option<ConcatBoxedInt>,
    pub(crate) char: Option<ConcatBoxedInt>,
    pub(crate) bool: Option<ConcatBoxedInt>,
}

/// Resolve `(class_id, compact_off, legacy_off)` for a wrapper whose sole
/// instance field is `value` (field index 0).
pub(crate) fn boxed_wrapper_layout(class_id: u32, expected_bytes: u32) -> Option<ConcatBoxedInt> {
    if class_id == 0 {
        return None;
    }
    let payload_off = if expected_bytes == 8 {
        cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET
    } else {
        cratonvm_types::FIELD_CELL_PAYLOAD32_OFFSET
    };
    let legacy_off = cratonvm_types::HEADER_SIZE as i32 + payload_off as i32;
    let mut compact_off = legacy_off;
    if cratonvm_types::compact_ref_fields_enabled() {
        if let Some((body_off, storage)) = cratonvm_types::compact_field_disp_storage(class_id, 0) {
            if storage.size_runtime() != expected_bytes {
                return None;
            }
            compact_off = body_off as i32;
        }
    }
    Some(ConcatBoxedInt {
        class_id,
        compact_off,
        legacy_off,
    })
}

pub(crate) fn resolve_boxed_layouts(
    plan: &JitConcatInlinePlan,
    class_id_for: impl Fn(i64) -> Option<u32>,
) -> ConcatBoxedLayouts {
    use crate::direct_helpers::{
        WELL_KNOWN_CLASS_BOOLEAN, WELL_KNOWN_CLASS_CHARACTER, WELL_KNOWN_CLASS_INTEGER,
        WELL_KNOWN_CLASS_LONG,
    };
    let mut out = ConcatBoxedLayouts::default();
    for seg in &plan.segments {
        if let ConcatSegment::Arg { kind, .. } = seg {
            match kind {
                ConcatArgKind::BoxedInt if out.int.is_none() => {
                    out.int = class_id_for(WELL_KNOWN_CLASS_INTEGER)
                        .and_then(|cid| boxed_wrapper_layout(cid, 4));
                }
                ConcatArgKind::BoxedLong if out.long.is_none() => {
                    out.long = class_id_for(WELL_KNOWN_CLASS_LONG)
                        .and_then(|cid| boxed_wrapper_layout(cid, 8));
                }
                ConcatArgKind::BoxedChar if out.char.is_none() => {
                    out.char = class_id_for(WELL_KNOWN_CLASS_CHARACTER)
                        .and_then(|cid| boxed_wrapper_layout(cid, 2));
                }
                ConcatArgKind::BoxedBool if out.bool.is_none() => {
                    out.bool = class_id_for(WELL_KNOWN_CLASS_BOOLEAN)
                        .and_then(|cid| boxed_wrapper_layout(cid, 1));
                }
                _ => {}
            }
        }
    }
    out
}

impl ConcatBoxedLayouts {
    pub(crate) fn all_resolved_for(&self, plan: &JitConcatInlinePlan) -> bool {
        plan.segments.iter().all(|seg| match seg {
            ConcatSegment::Arg { kind, .. } => match kind {
                ConcatArgKind::BoxedInt => self.int.is_some(),
                ConcatArgKind::BoxedLong => self.long.is_some(),
                ConcatArgKind::BoxedChar => self.char.is_some(),
                ConcatArgKind::BoxedBool => self.bool.is_some(),
                _ => true,
            },
            _ => true,
        })
    }
}

impl ConcatPassShape {
    /// The shape of `plan` over the operands staged at `arg_offsets`, sized
    /// against `alloc`.
    pub(super) fn new(
        plan: &'static JitConcatInlinePlan,
        outer: crate::StringFieldLayout,
        alloc: &SbToStringPlan,
        compact_flag_addr: usize,
        arg_offsets: Vec<i32>,
        class_id_off: i32,
    ) -> Self {
        Self {
            plan,
            outer,
            compact_flag_addr,
            arg_offsets,
            class_id_off,
            len_d8: alloc.len_d8,
            data_d8: alloc.data_d8,
            max_count: alloc.max_count,
            boxed_int: None,
            boxed_layouts: ConcatBoxedLayouts::default(),
        }
    }
}

// ---- encoders ---------------------------------------------------------------
//
// The subset of the single-pass `Compiler` emitters (`x64/emit.rs`) this code
// needs, with the SAME encodings, over a bare buffer.

/// ModRM (`mod=10`) + disp32 for `[base + disp32]`, with the index-free SIB
/// byte for an RSP/R12 base (`Compiler::emit_modrm_sib_disp32`).
fn modrm_sib_disp32(buf: &mut ExecutableBuffer, reg: u8, base: u8, disp: i32) {
    buf.emit_byte(0x80 | ((reg & 7) << 3) | (base & 7));
    if base & 7 == 0b100 {
        buf.emit_byte(0x24);
    }
    buf.emit(&disp.to_le_bytes());
}

fn rex_w_rb(reg: u8, rm: u8) -> u8 {
    0x48 | if reg >= 8 { 0x04 } else { 0 } | if rm >= 8 { 0x01 } else { 0 }
}

/// `MOV r64, imm64`, always the 10-byte form.
pub(crate) fn mov_imm64_full(buf: &mut ExecutableBuffer, reg: u8, imm: i64) {
    buf.emit_byte(if reg >= 8 { 0x49 } else { 0x48 });
    buf.emit_byte(0xB8 + (reg & 7));
    buf.emit(&imm.to_le_bytes());
}

/// `MOV reg, imm32` sign-extended (`Compiler::emit_mov_imm32_sx`).
fn mov_imm32_sx(buf: &mut ExecutableBuffer, reg: u8, imm: i32) {
    if imm == 0 {
        if reg >= 8 {
            buf.emit_byte(0x45);
        }
        buf.emit_byte(0x31); // XOR r32,r32
        buf.emit_byte(0xC0 | ((reg & 7) << 3) | (reg & 7));
    } else if imm > 0 {
        if reg >= 8 {
            buf.emit_byte(0x41);
        }
        buf.emit_byte(0xB8 + (reg & 7));
        buf.emit(&imm.to_le_bytes());
    } else {
        buf.emit_byte(if reg >= 8 { 0x49 } else { 0x48 });
        buf.emit_byte(0xC7);
        buf.emit_byte(0xC0 | (reg & 7));
        buf.emit(&imm.to_le_bytes());
    }
}

/// `MOV r64, [base + disp32]`.
pub(crate) fn mov_r64_mem(buf: &mut ExecutableBuffer, dst: u8, base: u8, disp: i32) {
    buf.emit_byte(rex_w_rb(dst, base));
    buf.emit_byte(0x8B);
    modrm_sib_disp32(buf, dst, base, disp);
}

/// `MOV r32, [base + disp32]` (zero-extends).
fn mov_r32_mem(buf: &mut ExecutableBuffer, dst: u8, base: u8, disp: i32) {
    if dst >= 8 || base >= 8 {
        buf.emit_byte(0x40 | if dst >= 8 { 0x04 } else { 0 } | if base >= 8 { 0x01 } else { 0 });
    }
    buf.emit_byte(0x8B);
    modrm_sib_disp32(buf, dst, base, disp);
}

/// `MOVZX r64, BYTE [base + disp32]`.
fn movzx8_r64_mem(buf: &mut ExecutableBuffer, dst: u8, base: u8, disp: i32) {
    buf.emit_byte(rex_w_rb(dst, base));
    buf.emit(&[0x0F, 0xB6]);
    modrm_sib_disp32(buf, dst, base, disp);
}

/// `MOV [base + disp32], r64`.
fn mov_mem_r64(buf: &mut ExecutableBuffer, base: u8, src: u8, disp: i32) {
    buf.emit_byte(rex_w_rb(src, base));
    buf.emit_byte(0x89);
    modrm_sib_disp32(buf, src, base, disp);
}

/// `MOV DWORD [base + disp32], imm32`.
fn mov_dword_mem_imm32(buf: &mut ExecutableBuffer, base: u8, disp: i32, imm: i32) {
    if base >= 8 {
        buf.emit_byte(0x41);
    }
    buf.emit_byte(0xC7);
    modrm_sib_disp32(buf, 0, base, disp);
    buf.emit(&imm.to_le_bytes());
}

/// `MOV BYTE [base + disp32], imm8`.
fn mov_byte_mem_imm8(buf: &mut ExecutableBuffer, base: u8, disp: i32, imm: u8) {
    if base >= 8 {
        buf.emit_byte(0x41);
    }
    buf.emit_byte(0xC6);
    modrm_sib_disp32(buf, 0, base, disp);
    buf.emit_byte(imm);
}

/// `LEA r64, [base + disp32]`.
fn lea_r64_mem(buf: &mut ExecutableBuffer, dst: u8, base: u8, disp: i32) {
    buf.emit_byte(rex_w_rb(dst, base));
    buf.emit_byte(0x8D);
    modrm_sib_disp32(buf, dst, base, disp);
}

/// `CMP r64, [base + disp32]`.
fn cmp_r64_mem(buf: &mut ExecutableBuffer, lhs: u8, base: u8, disp: i32) {
    buf.emit_byte(rex_w_rb(lhs, base));
    buf.emit_byte(0x3B);
    modrm_sib_disp32(buf, lhs, base, disp);
}

/// `SUB r64, [base + disp32]`.
fn sub_r64_mem(buf: &mut ExecutableBuffer, dst: u8, base: u8, disp: i32) {
    buf.emit_byte(rex_w_rb(dst, base));
    buf.emit_byte(0x2B);
    modrm_sib_disp32(buf, dst, base, disp);
}

/// `AND r64, [base + disp32]`.
fn and_r64_mem(buf: &mut ExecutableBuffer, dst: u8, base: u8, disp: i32) {
    buf.emit_byte(rex_w_rb(dst, base));
    buf.emit_byte(0x23);
    modrm_sib_disp32(buf, dst, base, disp);
}

/// `CMP BYTE [base + disp32], imm8`.
fn cmp_mem8_imm8(buf: &mut ExecutableBuffer, base: u8, disp: i32, imm: u8) {
    if base >= 8 {
        buf.emit_byte(0x41);
    }
    buf.emit_byte(0x80);
    modrm_sib_disp32(buf, 7, base, disp);
    buf.emit_byte(imm);
}

/// `TEST BYTE [base + disp32], imm8`.
fn test_mem8_imm8(buf: &mut ExecutableBuffer, base: u8, disp: i32, imm: u8) {
    if base >= 8 {
        buf.emit_byte(0x41);
    }
    buf.emit_byte(0xF6);
    modrm_sib_disp32(buf, 0, base, disp);
    buf.emit_byte(imm);
}

/// `MOV r64, r64` (nothing for a self-move).
fn mov_r64_r64(buf: &mut ExecutableBuffer, dst: u8, src: u8) {
    if dst == src {
        return;
    }
    buf.emit_byte(rex_w_rb(src, dst));
    buf.emit_byte(0x89);
    buf.emit_byte(0xC0 | ((src & 7) << 3) | (dst & 7));
}

/// `TEST r64, r64`.
fn test_r64_r64(buf: &mut ExecutableBuffer, reg: u8) {
    buf.emit_byte(if reg >= 8 { 0x4D } else { 0x48 });
    buf.emit_byte(0x85);
    buf.emit_byte(0xC0 | ((reg & 7) << 3) | (reg & 7));
}

/// `ADD r64, imm8` / `AND r64, imm8` (sign-extended).
fn alu_r64_imm8(buf: &mut ExecutableBuffer, ext: u8, reg: u8, imm: i8) {
    buf.emit_byte(if reg >= 8 { 0x49 } else { 0x48 });
    buf.emit_byte(0x83);
    buf.emit_byte(0xC0 | (ext << 3) | (reg & 7));
    buf.emit_byte(imm as u8); // Cast: x86-64 immediate encoding
}

fn add_r64_imm8(buf: &mut ExecutableBuffer, reg: u8, imm: i8) {
    alu_r64_imm8(buf, 0, reg, imm);
}

fn and_r64_imm8(buf: &mut ExecutableBuffer, reg: u8, imm: i8) {
    alu_r64_imm8(buf, 4, reg, imm);
}

/// `TEST r64, imm32`.
fn test_r64_imm32(buf: &mut ExecutableBuffer, reg: u8, imm: i32) {
    buf.emit_byte(if reg >= 8 { 0x49 } else { 0x48 });
    buf.emit_byte(0xF7);
    buf.emit_byte(0xC0 | (reg & 7));
    buf.emit(&imm.to_le_bytes());
}

/// `SHR r64, imm8`.
fn shr_r64_imm8(buf: &mut ExecutableBuffer, reg: u8, shift: u8) {
    buf.emit_byte(if reg >= 8 { 0x49 } else { 0x48 });
    buf.emit_byte(0xC1);
    buf.emit_byte(0xE8 | (reg & 7));
    buf.emit_byte(shift);
}

/// `SUB r64, r64` / `XOR r64, r64` (`op` = 0x29 / 0x31).
fn alu_r64_r64(buf: &mut ExecutableBuffer, op: u8, dst: u8, src: u8) {
    buf.emit_byte(rex_w_rb(src, dst));
    buf.emit_byte(op);
    buf.emit_byte(0xC0 | ((src & 7) << 3) | (dst & 7));
}

/// `MOV r64, [rbp - off]`, the shortest displacement form.
pub(crate) fn load_frame(buf: &mut ExecutableBuffer, reg: u8, off: i32) {
    buf.emit_byte(if reg >= 8 { 0x4C } else { 0x48 });
    buf.emit_byte(0x8B);
    let d = -i64::from(off);
    if let Ok(d8) = i8::try_from(d) {
        buf.emit_byte(0x45 | ((reg & 7) << 3));
        buf.emit_byte(d8 as u8); // Cast: the disp8's bits
    } else if let Ok(d32) = i32::try_from(d) {
        buf.emit_byte(0x85 | ((reg & 7) << 3));
        buf.emit(&d32.to_le_bytes());
    } else {
        buf.mark_codegen_unencodable("concat-frame-displacement-unencodable");
    }
}

/// `Jcc rel32` with a placeholder; answers the displacement's offset.
pub(crate) fn jcc_patch(buf: &mut ExecutableBuffer, cc: u8) -> usize {
    buf.emit(&[0x0F, cc]);
    let p = buf.pos();
    buf.emit(&[0; 4]);
    p
}

/// `JMP rel32` with a placeholder; answers the displacement's offset.
pub(crate) fn jmp_patch(buf: &mut ExecutableBuffer) -> usize {
    buf.emit_byte(0xE9);
    let p = buf.pos();
    buf.emit(&[0; 4]);
    p
}

/// Bind a rel32 placeholder to the current position. On an overflowed buffer
/// `try_patch_i32` sets the sticky flag and the compile is discarded.
pub(crate) fn patch_here(buf: &mut ExecutableBuffer, patch: usize) {
    let rel = buf.pos() as i32 - (patch as i32 + 4); // Cast: rel32 displacement
    let _ = buf.try_patch_i32(patch, rel);
}

/// A backward `JMP`/`Jcc` (already emitted, displacement at `patch`) to `top`.
fn patch_back(buf: &mut ExecutableBuffer, patch: usize, top: usize) {
    let rel = top as i32 - (patch as i32 + 4); // Cast: rel32 displacement
    let _ = buf.try_patch_i32(patch, rel);
}

// ---- CONCAT21-3: float / double operands ----------------------------------------
//
// JIT round 21 wave 5, lane concat (`CRATONVM_JIT_CONCAT_FP_INLINE`, opt-in).
// A plan with `F` / `D` operands renders each ONCE, before the length pass,
// through a leaf call to `concat_inline_plan::jit_concat_render_fp` into an
// RSP-relative scratch area:
//
// ```text
//   MOV RAX,RSP ; SUB RSP,reserve ; AND RSP,-16 ; MOV [RSP+save],RAX
//   per FP operand j:  a0 = bits [rbp-off] ; a1 = is_double ; a2 = &text[j]
//                      CALL render ; MOV [RSP+len[j]],RAX ; CMP RAX,32 ; JA decline
//   ... length pass (R8 += len[j]) ... tail ... fill (copy / inflate text[j]) ...
//   MOV RSP,[RSP+save]                     ; on the fast path, and on EVERY
//                                          ; decline edge (the caller's stub)
// ```
//
// Area (from the aligned RSP up): 32 bytes of Win64 shadow space, then 48 bytes
// per FP operand (32 text + 8 length + 8 pad), then the saved RSP. The leaf
// call clobbers what the decline edge's bridge CALL already clobbers (the
// caller-saved GPRs and XMM registers), so the node's register contract is
// unchanged; it runs no Java code, polls nothing and touches no VM state, so
// the fast path still owes no safepoint map and no deopt state. Every pass
// address stays `[rbp - off]` (operands) or a register (the tails), so moving
// RSP changes nothing they read.

const RSP: u8 = 4;
#[cfg(windows)]
const FP_A0: u8 = 1; // RCX
#[cfg(windows)]
const FP_A1: u8 = 2; // RDX
#[cfg(windows)]
const FP_A2: u8 = 8; // R8
#[cfg(not(windows))]
const FP_A0: u8 = 7; // RDI
#[cfg(not(windows))]
const FP_A1: u8 = 6; // RSI
#[cfg(not(windows))]
const FP_A2: u8 = 2; // RDX

const FP_SHADOW: i32 = 32;
const FP_SLOT: i32 = 48;

/// The FP operands of `plan`, in segment order.
fn fp_count(plan: &JitConcatInlinePlan) -> usize {
    plan.segments
        .iter()
        .filter(|s| {
            matches!(
                s,
                ConcatSegment::Arg {
                    kind: ConcatArgKind::Float | ConcatArgKind::Double,
                    ..
                }
            )
        })
        .count()
}

/// The scratch slot of operand `index` (its ordinal among the FP operands).
fn fp_ordinal(plan: &JitConcatInlinePlan, index: usize) -> i32 {
    let mut j = 0i32;
    for seg in &plan.segments {
        if let ConcatSegment::Arg { index: i, kind } = *seg {
            if i == index {
                break;
            }
            if matches!(kind, ConcatArgKind::Float | ConcatArgKind::Double) {
                j += 1;
            }
        }
    }
    j
}

fn fp_text_off(j: i32) -> i32 {
    FP_SHADOW + FP_SLOT * j
}

fn fp_len_off(j: i32) -> i32 {
    fp_text_off(j) + 32
}

fn fp_save_off(n: i32) -> i32 {
    FP_SHADOW + FP_SLOT * n
}

/// `SUB RSP` amount: the area plus 16 bytes of alignment slack.
fn fp_reserve(n: i32) -> i32 {
    fp_save_off(n) + 32
}

/// CONCAT21-3: the scratch prologue of a plan with FP operands (nothing for
/// any other plan). Every edge it pushes onto `decline` leaves with RSP still
/// moved: the caller lands them on a stub that runs [`emit_fp_restore`] first.
/// Clobbers every caller-saved register (the leaf call).
pub(crate) fn emit_fp_prologue(
    buf: &mut ExecutableBuffer,
    s: &ConcatPassShape,
    decline: &mut Vec<usize>,
) {
    // Cast: at most CONCAT_INLINE_MAX_ARGS.
    let n = fp_count(s.plan) as i32;
    if n == 0 {
        return;
    }
    mov_r64_r64(buf, RAX, RSP);
    buf.emit(&[0x48, 0x81, 0xEC]); // SUB RSP,imm32
    buf.emit(&fp_reserve(n).to_le_bytes());
    and_r64_imm8(buf, RSP, -16);
    mov_mem_r64(buf, RSP, RAX, fp_save_off(n));
    let render = crate::concat_inline_plan::jit_concat_render_fp
        as unsafe extern "C" fn(u64, u64, *mut u8) -> u64;
    let mut j = 0i32;
    for seg in &s.plan.segments {
        let ConcatSegment::Arg { index, kind } = *seg else {
            continue;
        };
        let is_double = match kind {
            ConcatArgKind::Float => 0,
            ConcatArgKind::Double => 1,
            _ => continue,
        };
        load_frame(buf, FP_A0, s.arg_offsets[index]);
        mov_imm32_sx(buf, FP_A1, is_double);
        lea_r64_mem(buf, FP_A2, RSP, fp_text_off(j));
        // Cast: the leaf's address, baked as an imm64.
        mov_imm64_full(buf, RAX, render as usize as i64);
        buf.emit(&[0xFF, 0xD0]); // CALL RAX
        mov_mem_r64(buf, RSP, RAX, fp_len_off(j));
        buf.emit(&[0x48, 0x83, 0xF8, 0x20]); // CMP RAX,32 (CONCAT_FP_TEXT_MAX)
        decline.push(jcc_patch(buf, 0x87)); // JA: no text (u64::MAX) -> bridge
        j += 1;
    }
}

/// CONCAT21-3: `MOV RSP,[RSP+save]` -- undo [`emit_fp_prologue`] (nothing for
/// a plan without FP operands). Touches no register but RSP.
pub(crate) fn emit_fp_restore(buf: &mut ExecutableBuffer, s: &ConcatPassShape) {
    // Cast: at most CONCAT_INLINE_MAX_ARGS.
    let n = fp_count(s.plan) as i32;
    if n == 0 {
        return;
    }
    mov_r64_mem(buf, RSP, RSP, fp_save_off(n));
}

/// `ADD R8,[RSP+len[j]]`: an FP operand's text length (length passes).
fn emit_fp_add_len(buf: &mut ExecutableBuffer, s: &ConcatPassShape, index: usize) {
    let j = fp_ordinal(s.plan, index);
    buf.emit_byte(rex_w_rb(R8, RSP));
    buf.emit_byte(0x03); // ADD r64, r/m64
    modrm_sib_disp32(buf, R8, RSP, fp_len_off(j));
}

/// RDX = &text[j], ECX = len[j] (fill passes; then a copy or an inflate).
fn emit_fp_text(buf: &mut ExecutableBuffer, s: &ConcatPassShape, index: usize) {
    let j = fp_ordinal(s.plan, index);
    lea_r64_mem(buf, RDX, RSP, fp_text_off(j));
    mov_r64_mem(buf, RCX, RSP, fp_len_off(j));
}

// ---- CONCAT21-4: an exact Integer in a String operand -------------------------

/// RAX = a non-null `String` operand. When it is an exact `Integer` object
/// (class id, then a plain-object kind tag: an `Integer[]` carries the same
/// id and declines), leaves RAX = its sign-extended `value` and falls through;
/// any other class jumps to the answered patch (the String path). With
/// `decline` (the length passes) the array case declines; without (the fill
/// passes, after the length pass proved the shape) it is not re-tested.
/// Clobbers RAX and the flags.
fn emit_bool_len(buf: &mut ExecutableBuffer) {
    // R8 += 4 + (low 32 bits == 0): "true" / "false".
    buf.emit(&[0x85, 0xC0]); // TEST EAX,EAX
    buf.emit(&[0x0F, 0x94, 0xC1]); // SETZ CL
    buf.emit(&[0x0F, 0xB6, 0xC9]); // MOVZX ECX,CL
    buf.emit(&[0x49, 0x01, 0xC8]); // ADD R8,RCX
    add_r64_imm8(buf, R8, 4);
}

fn emit_load_boxed_payload(buf: &mut ExecutableBuffer, off: i32, kind: ConcatArgKind) {
    match kind {
        ConcatArgKind::BoxedInt => {
            mov_r32_mem(buf, RAX, RAX, off);
            buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX,EAX
        }
        ConcatArgKind::BoxedLong => {
            buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX,[RAX+disp32]
            buf.emit(&off.to_le_bytes());
        }
        ConcatArgKind::BoxedChar => {
            buf.emit(&[0x0F, 0xB7, 0x80]); // MOVZX EAX, WORD [RAX+disp32]
            buf.emit(&off.to_le_bytes());
        }
        ConcatArgKind::BoxedBool => {
            buf.emit(&[0x0F, 0xB6, 0x80]); // MOVZX EAX, BYTE [RAX+disp32]
            buf.emit(&off.to_le_bytes());
        }
        _ => {
            buf.mark_codegen_unencodable("concat-invalid-boxed-kind");
        }
    }
}

fn emit_boxed_raw_load(
    buf: &mut ExecutableBuffer,
    s: &ConcatPassShape,
    b: ConcatBoxedInt,
    kind: ConcatArgKind,
    decline: Option<&mut Vec<usize>>,
) -> usize {
    buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
    let is_null = jcc_patch(buf, 0x84); // JZ
    buf.emit(&[0x81, 0xB8]); // CMP DWORD [RAX+class_id], imm32
    buf.emit(&s.class_id_off.to_le_bytes());
    buf.emit(&b.class_id.to_le_bytes());
    if let Some(decline) = decline {
        decline.push(jcc_patch(buf, 0x85)); // JNE: unexpected class
        test_mem8_imm8(
            buf,
            RAX,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as i32,
            cratonvm_types::KIND_TAG_BYTE_MASK,
        );
        decline.push(jcc_patch(buf, 0x85)); // JNZ: an array
    }
    test_mem8_imm8(
        buf,
        RAX,
        cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
        cratonvm_types::GC_FLAG_COMPACT,
    );
    let legacy = jcc_patch(buf, 0x84); // JZ legacy
    emit_load_boxed_payload(buf, b.compact_off, kind);
    let done = jmp_patch(buf);
    patch_here(buf, legacy);
    emit_load_boxed_payload(buf, b.legacy_off, kind);
    patch_here(buf, done);
    is_null
}

fn emit_boxed_len(
    buf: &mut ExecutableBuffer,
    s: &ConcatPassShape,
    b: Option<ConcatBoxedInt>,
    kind: ConcatArgKind,
    wide: bool,
    decline: &mut Vec<usize>,
    promote: Option<&mut Vec<usize>>,
) {
    let Some(b) = b else {
        buf.mark_codegen_unencodable("concat-missing-boxed-layout");
        return;
    };
    let is_null = emit_boxed_raw_load(buf, s, b, kind, Some(decline));
    match kind {
        ConcatArgKind::BoxedInt | ConcatArgKind::BoxedLong => emit_signed_digits_len(buf),
        ConcatArgKind::BoxedChar => {
            if !wide {
                buf.emit(&[0x3D, 0xFF, 0x00, 0x00, 0x00]); // CMP EAX,0xFF
                let utf16 = jcc_patch(buf, 0x87); // JA: UTF16
                match promote {
                    Some(p) => p.push(utf16),
                    None => decline.push(utf16),
                }
            }
            buf.emit(&[0x49, 0xFF, 0xC0]); // INC R8
        }
        ConcatArgKind::BoxedBool => emit_bool_len(buf),
        _ => {
            buf.mark_codegen_unencodable("concat-invalid-boxed-kind");
            return;
        }
    }
    let done = jmp_patch(buf);
    patch_here(buf, is_null);
    add_r64_imm8(buf, R8, 4); // "null"
    patch_here(buf, done);
}

fn emit_boxed_fill(
    buf: &mut ExecutableBuffer,
    s: &ConcatPassShape,
    b: Option<ConcatBoxedInt>,
    kind: ConcatArgKind,
    wide: bool,
) {
    let Some(b) = b else {
        buf.mark_codegen_unencodable("concat-missing-boxed-layout");
        return;
    };
    let is_null = emit_boxed_raw_load(buf, s, b, kind, None);
    match kind {
        ConcatArgKind::BoxedInt | ConcatArgKind::BoxedLong => {
            if wide {
                emit_decimal_wide(buf);
            } else {
                emit_decimal(buf);
            }
        }
        ConcatArgKind::BoxedChar => {
            if wide {
                buf.emit(&[0x66, 0x41, 0x89, 0x00]); // MOV [R8],AX
                add_r64_imm8(buf, R8, 2);
            } else {
                buf.emit(&[0x41, 0x88, 0x00]); // MOV [R8],AL
                buf.emit(&[0x49, 0xFF, 0xC0]); // INC R8
            }
        }
        ConcatArgKind::BoxedBool => {
            buf.emit(&[0x85, 0xC0]); // TEST EAX,EAX
            let is_false = jcc_patch(buf, 0x84); // JZ
            if wide {
                emit_literal(buf, &ascii_utf16le(b"true"));
            } else {
                emit_literal(buf, b"true");
            }
            let b_done = jmp_patch(buf);
            patch_here(buf, is_false);
            if wide {
                emit_literal(buf, &ascii_utf16le(b"false"));
            } else {
                emit_literal(buf, b"false");
            }
            patch_here(buf, b_done);
        }
        _ => {
            buf.mark_codegen_unencodable("concat-invalid-boxed-kind");
            return;
        }
    }
    let done = jmp_patch(buf);
    patch_here(buf, is_null);
    if wide {
        emit_literal(buf, &ascii_utf16le(b"null"));
    } else {
        emit_literal(buf, b"null");
    }
    patch_here(buf, done);
}

fn emit_boxed_int_load(
    buf: &mut ExecutableBuffer,
    s: &ConcatPassShape,
    b: ConcatBoxedInt,
    decline: Option<&mut Vec<usize>>,
) -> usize {
    buf.emit(&[0x81, 0xB8]); // CMP DWORD [RAX+class_id], imm32
    buf.emit(&s.class_id_off.to_le_bytes());
    buf.emit(&b.class_id.to_le_bytes());
    let not_box = jcc_patch(buf, 0x85); // JNE: the String path
    if let Some(decline) = decline {
        test_mem8_imm8(
            buf,
            RAX,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as i32, // Cast: small header constant
            cratonvm_types::KIND_TAG_BYTE_MASK,
        );
        decline.push(jcc_patch(buf, 0x85)); // JNZ: an array
    }
    test_mem8_imm8(
        buf,
        RAX,
        cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32, // Cast: small header constant
        cratonvm_types::GC_FLAG_COMPACT,
    );
    let legacy = jcc_patch(buf, 0x84); // JZ legacy
    mov_r32_mem(buf, RAX, RAX, b.compact_off);
    let done = jmp_patch(buf);
    patch_here(buf, legacy);
    mov_r32_mem(buf, RAX, RAX, b.legacy_off);
    patch_here(buf, done);
    buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX,EAX
    not_box
}

/// `R8 += sign + digits(RAX)` for a signed 64-bit RAX (the length passes'
/// `int` / `long` arm). Clobbers RAX, RDX and the flags.
fn emit_signed_digits_len(buf: &mut ExecutableBuffer) {
    buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
    let non_negative = jcc_patch(buf, 0x89); // JNS
    buf.emit(&[0x49, 0xFF, 0xC0]); // INC R8
    buf.emit(&[0x48, 0xF7, 0xD8]); // NEG RAX
    patch_here(buf, non_negative);
    emit_digit_count(buf, R8);
}

// ---- the two passes -----------------------------------------------------------

/// The length pass: R8 = the result's length in bytes, every operand
/// screened (see `op_concat.rs`'s module doc); ECX = R8 on the fall-through.
/// Every refusal is pushed onto `decline`, before anything is stored.
/// Clobbers RAX, RCX, RDX, R8, R11 and the flags.
///
/// A wide plan (STRGAP4-4) takes [`emit_length_pass_wide`] instead: R8 then
/// counts UTF-16 units and ECX = 2 * R8, the `byte[]`'s length.
pub(crate) fn emit_length_pass(
    buf: &mut ExecutableBuffer,
    s: &ConcatPassShape,
    decline: &mut Vec<usize>,
) {
    if s.plan.wide {
        emit_length_pass_wide(buf, s, decline);
        return;
    }
    emit_length_pass_latin1(buf, s, decline, None);
}

/// CONCAT21-1 (`CRATONVM_JIT_CONCAT_CODER_PROMOTE`): [`emit_length_pass`] of
/// a LATIN1 plan whose two UTF16 refusals -- a `String` operand with the
/// UTF16 coder, a `char` above `0xFF` -- are pushed onto `promote` instead of
/// `decline`, for the caller to land on the plan's wide twin's passes. Every
/// other refusal still goes to `decline`. Nothing is stored before either
/// edge, and the twin's length pass starts from scratch (it reloads every
/// operand), so an edge carries no register state. Same clobbers.
pub(crate) fn emit_length_pass_promoting(
    buf: &mut ExecutableBuffer,
    s: &ConcatPassShape,
    decline: &mut Vec<usize>,
    promote: &mut Vec<usize>,
) {
    if s.plan.wide {
        emit_length_pass_wide(buf, s, decline);
        return;
    }
    emit_length_pass_latin1(buf, s, decline, Some(promote));
}

/// The LATIN1 length pass; `promote` (CONCAT21-1) takes the UTF16 edges.
fn emit_length_pass_latin1(
    buf: &mut ExecutableBuffer,
    s: &ConcatPassShape,
    decline: &mut Vec<usize>,
    mut promote: Option<&mut Vec<usize>>,
) {
    // Compact strings off: every String is UTF16, so no LATIN1 result.
    // Cast: the flag's address, baked as an imm64.
    mov_imm64_full(buf, R11, s.compact_flag_addr as i64);
    cmp_mem8_imm8(buf, R11, 0, 0);
    decline.push(jcc_patch(buf, 0x84)); // JE -> bridge
                                        // Cast: bounded by CONCAT_INLINE_MAX_LITERAL.
    mov_imm32_sx(buf, R8, s.plan.literal_len as i32);
    for seg in &s.plan.segments {
        let ConcatSegment::Arg { index, kind } = *seg else {
            continue;
        };
        let off = s.arg_offsets[index];
        load_frame(buf, RAX, off);
        match kind {
            ConcatArgKind::String => {
                test_r64_r64(buf, RAX);
                let non_null = jcc_patch(buf, 0x85); // JNZ
                add_r64_imm8(buf, R8, 4); // "null"
                let next = jmp_patch(buf);
                patch_here(buf, non_null);
                // CONCAT21-4: a fused `valueOf`'s exact Integer.
                let mut boxed_next = None;
                if let Some(b) = s.boxed_int {
                    let not_box = emit_boxed_int_load(buf, s, b, Some(&mut *decline));
                    emit_signed_digits_len(buf);
                    boxed_next = Some(jmp_patch(buf));
                    patch_here(buf, not_box);
                }
                emit_string_value_screened(
                    buf,
                    s,
                    Some(&mut *decline),
                    false,
                    promote.as_deref_mut(),
                );
                // ECX = value.length (zero-extends) ; R8 += RCX.
                mov_r32_mem(buf, RCX, RDX, i32::from(s.len_d8));
                buf.emit(&[0x49, 0x01, 0xC8]); // ADD R8,RCX
                patch_here(buf, next);
                if let Some(j) = boxed_next {
                    patch_here(buf, j);
                }
            }
            ConcatArgKind::Int | ConcatArgKind::Long => {
                if kind == ConcatArgKind::Int {
                    buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX,EAX
                }
                // A sign, then the magnitude's digits.
                buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
                let non_negative = jcc_patch(buf, 0x89); // JNS
                buf.emit(&[0x49, 0xFF, 0xC0]); // INC R8
                buf.emit(&[0x48, 0xF7, 0xD8]); // NEG RAX
                patch_here(buf, non_negative);
                emit_digit_count(buf, R8);
            }
            ConcatArgKind::Char => {
                buf.emit(&[0x0F, 0xB7, 0xC0]); // MOVZX EAX,AX
                buf.emit(&[0x3D, 0xFF, 0x00, 0x00, 0x00]); // CMP EAX,0xFF
                let utf16 = jcc_patch(buf, 0x87); // JA: UTF16
                match promote.as_deref_mut() {
                    Some(p) => p.push(utf16),
                    None => decline.push(utf16),
                }
                buf.emit(&[0x49, 0xFF, 0xC0]); // INC R8
            }
            ConcatArgKind::Boolean => emit_bool_len(buf),
            ConcatArgKind::BoxedInt => emit_boxed_len(
                buf,
                s,
                s.boxed_layouts.int,
                kind,
                false,
                decline,
                promote.as_deref_mut(),
            ),
            ConcatArgKind::BoxedLong => emit_boxed_len(
                buf,
                s,
                s.boxed_layouts.long,
                kind,
                false,
                decline,
                promote.as_deref_mut(),
            ),
            ConcatArgKind::BoxedChar => emit_boxed_len(
                buf,
                s,
                s.boxed_layouts.char,
                kind,
                false,
                decline,
                promote.as_deref_mut(),
            ),
            ConcatArgKind::BoxedBool => emit_boxed_len(
                buf,
                s,
                s.boxed_layouts.bool,
                kind,
                false,
                decline,
                promote.as_deref_mut(),
            ),
            // CONCAT21-3: the text the prologue rendered (ASCII).
            ConcatArgKind::Float | ConcatArgKind::Double => emit_fp_add_len(buf, s, index),
        }
    }
    if s.plan.literal_len == 0 {
        // An empty result keeps the bridge (its answer, unchanged).
        buf.emit(&[0x4D, 0x85, 0xC0]); // TEST R8,R8
        decline.push(jcc_patch(buf, 0x84)); // JZ
    }
    // CMP R8,max ; JA -> bridge (unsigned).
    buf.emit(&[0x49, 0x81, 0xF8]);
    buf.emit(&s.max_count.to_le_bytes());
    decline.push(jcc_patch(buf, 0x87));
    buf.emit(&[0x44, 0x89, 0xC1]); // MOV ECX,R8D
}

/// RAX = a non-null `String` operand; leaves RDX = its `value` array, by the
/// instance's own layout (compact or legacy). With `decline` it also proves
/// the shape the fill pass relies on -- exact `java/lang/String`, LATIN1
/// coder, a non-null `byte[]` -- declining otherwise; without it (the fill
/// pass, after the length pass proved it) it only loads. Clobbers RCX, RDX
/// and the flags.
fn emit_string_value(
    buf: &mut ExecutableBuffer,
    s: &ConcatPassShape,
    decline: Option<&mut Vec<usize>>,
) {
    emit_string_value_coded(buf, s, decline, false);
}

/// [`emit_string_value`], and with `wide` (STRGAP4-4, a wide plan's passes)
/// it also leaves ECX = the coder (0 LATIN1, 1 UTF16) on both the checked and
/// the unchecked form, and the checked form admits either coder (declining
/// any other value) instead of LATIN1 only. Clobbers RCX, RDX and the flags.
fn emit_string_value_coded(
    buf: &mut ExecutableBuffer,
    s: &ConcatPassShape,
    decline: Option<&mut Vec<usize>>,
    wide: bool,
) {
    emit_string_value_screened(buf, s, decline, wide, None);
}

/// [`emit_string_value_coded`]; on the checked LATIN1 form, `utf16_edge`
/// (CONCAT21-1) takes the "UTF16 coder" refusal instead of `decline`.
fn emit_string_value_screened(
    buf: &mut ExecutableBuffer,
    s: &ConcatPassShape,
    decline: Option<&mut Vec<usize>>,
    wide: bool,
    utf16_edge: Option<&mut Vec<usize>>,
) {
    let outer = &s.outer;
    let checked = decline.is_some();
    let load_coder = checked || wide;
    let mut edges: Vec<usize> = Vec::new();
    if checked {
        // CMP DWORD [RAX+class_id],String: the operand is typed String, so
        // this holds for every verified class file.
        buf.emit(&[0x81, 0xB8]); // CMP r/m32 (/7, [RAX+disp32]), imm32
        buf.emit(&s.class_id_off.to_le_bytes());
        // Cast: a ClassId fits in 32 bits.
        buf.emit(&(outer.string_class_id as i32).to_le_bytes());
        edges.push(jcc_patch(buf, 0x85)); // JNE -> bridge
    }
    test_mem8_imm8(
        buf,
        RAX,
        cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32, // Cast: small header constant
        cratonvm_types::GC_FLAG_COMPACT,
    );
    let legacy = jcc_patch(buf, 0x84); // JZ legacy
    if load_coder {
        if outer.coder_compact_is_byte {
            movzx8_r64_mem(buf, RCX, RAX, outer.coder_compact_offset);
        } else {
            mov_r32_mem(buf, RCX, RAX, outer.coder_compact_offset);
        }
    }
    mov_r64_mem(buf, RDX, RAX, outer.value_compact_offset);
    let joined = jmp_patch(buf);
    patch_here(buf, legacy);
    if load_coder {
        mov_r32_mem(buf, RCX, RAX, outer.coder_legacy_offset);
    }
    mov_r64_mem(buf, RDX, RAX, outer.value_legacy_offset);
    patch_here(buf, joined);
    if let Some(decline) = decline {
        decline.append(&mut edges);
        if wide {
            buf.emit(&[0x83, 0xF9, 0x01]); // CMP ECX,1: LATIN1 0, UTF16 1
            decline.push(jcc_patch(buf, 0x87)); // JA: no such coder
        } else {
            buf.emit(&[0x85, 0xC9]); // TEST ECX,ECX: LATIN1 is 0
            let utf16 = jcc_patch(buf, 0x85); // JNZ: UTF16
            match utf16_edge {
                Some(p) => p.push(utf16),
                None => decline.push(utf16),
            }
        }
        test_r64_r64(buf, RDX);
        decline.push(jcc_patch(buf, 0x84)); // JZ: no value
                                            // The `byte[]` kind tag (a `char[]` value is not this layout).
        let kind_d8 = i32::try_from(cratonvm_types::KIND_TAGS_BYTE_OFFSET).unwrap_or(0);
        let tag = cratonvm_types::primitive_array_kind_tags_byte("[B").unwrap_or(0);
        cmp_mem8_imm8(buf, RDX, kind_d8, tag);
        decline.push(jcc_patch(buf, 0x85)); // JNE
    }
}

/// The fill pass: on entry R11 = the fresh `byte[]` (exactly the length
/// pass's byte count), R9 = the String. Writes every segment in recipe order
/// through the cursor R8. Clobbers RAX, RCX, RDX, R8, R10 and the flags.
pub(crate) fn emit_fill_pass(buf: &mut ExecutableBuffer, s: &ConcatPassShape) {
    if s.plan.wide {
        emit_fill_pass_wide(buf, s);
        return;
    }
    let data = i32::from(s.data_d8);
    let len_d8 = i32::from(s.len_d8);
    lea_r64_mem(buf, R8, R11, data);
    for seg in &s.plan.segments {
        match seg {
            ConcatSegment::Literal(bytes) => emit_literal(buf, bytes),
            // Only a wide plan carries one, and it took the branch above; a
            // plan that broke that invariant discards the compile.
            ConcatSegment::WideLiteral(_) => {
                buf.mark_codegen_unencodable("concat-wide-literal-in-latin1-plan");
            }
            ConcatSegment::Arg { index, kind } => {
                load_frame(buf, RAX, s.arg_offsets[*index]);
                match kind {
                    ConcatArgKind::String => {
                        test_r64_r64(buf, RAX);
                        let non_null = jcc_patch(buf, 0x85); // JNZ
                        emit_literal(buf, b"null");
                        let next = jmp_patch(buf);
                        patch_here(buf, non_null);
                        // CONCAT21-4: a fused `valueOf`'s exact Integer.
                        let mut boxed_next = None;
                        if let Some(b) = s.boxed_int {
                            let not_box = emit_boxed_int_load(buf, s, b, None);
                            emit_decimal(buf);
                            boxed_next = Some(jmp_patch(buf));
                            patch_here(buf, not_box);
                        }
                        emit_string_value(buf, s, None);
                        mov_r32_mem(buf, RCX, RDX, len_d8);
                        lea_r64_mem(buf, RDX, RDX, data);
                        emit_copy(buf);
                        patch_here(buf, next);
                        if let Some(j) = boxed_next {
                            patch_here(buf, j);
                        }
                    }
                    ConcatArgKind::Int | ConcatArgKind::Long => {
                        if *kind == ConcatArgKind::Int {
                            buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX,EAX
                        }
                        emit_decimal(buf);
                    }
                    ConcatArgKind::Char => {
                        buf.emit(&[0x41, 0x88, 0x00]); // MOV [R8],AL
                        buf.emit(&[0x49, 0xFF, 0xC0]); // INC R8
                    }
                    ConcatArgKind::Boolean => {
                        buf.emit(&[0x85, 0xC0]); // TEST EAX,EAX
                        let is_false = jcc_patch(buf, 0x84); // JZ
                        emit_literal(buf, b"true");
                        let next = jmp_patch(buf);
                        patch_here(buf, is_false);
                        emit_literal(buf, b"false");
                        patch_here(buf, next);
                    }
                    ConcatArgKind::BoxedInt => {
                        emit_boxed_fill(buf, s, s.boxed_layouts.int, *kind, false)
                    }
                    ConcatArgKind::BoxedLong => {
                        emit_boxed_fill(buf, s, s.boxed_layouts.long, *kind, false)
                    }
                    ConcatArgKind::BoxedChar => {
                        emit_boxed_fill(buf, s, s.boxed_layouts.char, *kind, false)
                    }
                    ConcatArgKind::BoxedBool => {
                        emit_boxed_fill(buf, s, s.boxed_layouts.bool, *kind, false)
                    }
                    // CONCAT21-3: copy the prologue's text.
                    ConcatArgKind::Float | ConcatArgKind::Double => {
                        emit_fp_text(buf, s, *index);
                        emit_copy(buf);
                    }
                }
            }
        }
    }
}

// ---- the renderers --------------------------------------------------------------

/// `acc += digits(RAX)` for an unsigned magnitude in RAX (`1` for zero).
/// `acc` is R8 (the length pass) or RCX (the fill pass, zeroed first by the
/// caller). Clobbers RDX and the flags; RAX is unchanged.
///
/// The power of ten in RDX never overflows before the loop ends: the largest
/// magnitude is `2^63 < 10^19 < 2^64`.
pub(crate) fn emit_digit_count(buf: &mut ExecutableBuffer, acc: u8) {
    let inc_acc: &[u8] = if acc == R8 {
        &[0x49, 0xFF, 0xC0] // INC R8
    } else {
        &[0x48, 0xFF, 0xC1] // INC RCX
    };
    buf.emit(inc_acc);
    buf.emit(&[0xBA, 0x0A, 0x00, 0x00, 0x00]); // MOV EDX,10
    let top = buf.pos();
    buf.emit(&[0x48, 0x39, 0xD0]); // CMP RAX,RDX
    let done = jcc_patch(buf, 0x82); // JB: fewer digits
    buf.emit(inc_acc);
    buf.emit(&[0x48, 0x6B, 0xD2, 0x0A]); // IMUL RDX,RDX,10
    let back = jmp_patch(buf);
    patch_back(buf, back, top);
    patch_here(buf, done);
}

/// The decimal text of the signed 64-bit RAX at [R8], R8 past it:
/// `Long.toString`'s spelling (and `Integer.toString`'s, for a sign-extended
/// `int`). Digits are written backwards from the end with a
/// multiply-by-reciprocal divide. Clobbers RAX, RCX, RDX, R10 and the flags.
pub(crate) fn emit_decimal(buf: &mut ExecutableBuffer) {
    buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
    let non_negative = jcc_patch(buf, 0x89); // JNS
    mov_byte_mem_imm8(buf, R8, 0, b'-');
    buf.emit(&[0x49, 0xFF, 0xC0]); // INC R8
    buf.emit(&[0x48, 0xF7, 0xD8]); // NEG RAX (MIN_VALUE: 2^63 unsigned)
    patch_here(buf, non_negative);
    buf.emit(&[0x31, 0xC9]); // XOR ECX,ECX
    emit_digit_count(buf, RCX);
    buf.emit(&[0x49, 0x01, 0xC8]); // ADD R8,RCX: past the digits
    buf.emit(&[0x4D, 0x89, 0xC2]); // MOV R10,R8: write backwards
    emit_unsigned_decimal_backwards(buf);
}

/// RAX is the unsigned magnitude, R10 the end cursor. R8 stays at the end.
fn emit_unsigned_decimal_backwards(buf: &mut ExecutableBuffer) {
    let top = buf.pos();
    buf.emit(&[0x48, 0x89, 0xC1]); // MOV RCX,RAX
                                   // Cast: the reciprocal's bits as an imm64.
    mov_imm64_full(buf, RDX, CONCAT_DIV10_MAGIC as i64);
    buf.emit(&[0x48, 0xF7, 0xE2]); // MUL RDX: RDX:RAX = x * magic
    buf.emit(&[0x48, 0xC1, 0xEA, 0x03]); // SHR RDX,3: q = x / 10
    buf.emit(&[0x48, 0x8D, 0x04, 0x92]); // LEA RAX,[RDX+RDX*4]
    buf.emit(&[0x48, 0x01, 0xC0]); // ADD RAX,RAX: 10q
    buf.emit(&[0x48, 0x29, 0xC1]); // SUB RCX,RAX: x % 10
    buf.emit(&[0x83, 0xC1, 0x30]); // ADD ECX,'0'
    buf.emit(&[0x49, 0xFF, 0xCA]); // DEC R10
    buf.emit(&[0x41, 0x88, 0x0A]); // MOV [R10],CL
    buf.emit(&[0x48, 0x89, 0xD0]); // MOV RAX,RDX
    buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
    let back = jcc_patch(buf, 0x85); // JNZ top
    patch_back(buf, back, top);
}

/// Literal bytes at [R8], then R8 += their count: 8, 4, 2 and 1-byte
/// immediate stores, never past the literal (the String follows the array).
/// Clobbers RAX.
pub(crate) fn emit_literal(buf: &mut ExecutableBuffer, bytes: &[u8]) {
    let mut at = 0usize;
    while at < bytes.len() {
        let rest = bytes.len() - at;
        // Cast: bounded by CONCAT_INLINE_MAX_LITERAL (or 5).
        let disp = at as i32;
        if rest >= 8 {
            let mut w = [0u8; 8];
            w.copy_from_slice(&bytes[at..at + 8]);
            mov_imm64_full(buf, RAX, i64::from_le_bytes(w));
            mov_mem_r64(buf, R8, RAX, disp);
            at += 8;
        } else if rest >= 4 {
            let mut w = [0u8; 4];
            w.copy_from_slice(&bytes[at..at + 4]);
            mov_dword_mem_imm32(buf, R8, disp, i32::from_le_bytes(w));
            at += 4;
        } else if rest >= 2 {
            // MOV WORD [R8+disp32],imm16: 66 REX.B C7 /0.
            buf.emit(&[0x66, 0x41, 0xC7, 0x80]);
            buf.emit(&disp.to_le_bytes());
            buf.emit(&bytes[at..at + 2]);
            at += 2;
        } else {
            mov_byte_mem_imm8(buf, R8, disp, bytes[at]);
            at += 1;
        }
    }
    // Cast: bounded by CONCAT_INLINE_MAX_LITERAL.
    let n = bytes.len() as i32;
    if n <= 127 {
        // Cast: checked just above.
        add_r64_imm8(buf, R8, n as i8);
    } else {
        buf.emit(&[0x49, 0x81, 0xC0]); // ADD R8,imm32
        buf.emit(&n.to_le_bytes());
    }
}

/// ECX bytes from [RDX] to [R8]; both pointers end past the copy. 8 at a
/// time, then the 0..=7 left as one 4-, 2- and 1-byte step each by ECX's low
/// bits: neither side is read or written past ECX. Clobbers RAX, RCX, RDX and
/// the flags.
pub(crate) fn emit_copy(buf: &mut ExecutableBuffer) {
    let qword_top = buf.pos();
    buf.emit(&[0x83, 0xF9, 0x08]); // CMP ECX,8
    let to_bytes = jcc_patch(buf, 0x82); // JB
    buf.emit(&[0x48, 0x8B, 0x02]); // MOV RAX,[RDX]
    buf.emit(&[0x49, 0x89, 0x00]); // MOV [R8],RAX
    buf.emit(&[0x48, 0x83, 0xC2, 0x08]); // ADD RDX,8
    buf.emit(&[0x49, 0x83, 0xC0, 0x08]); // ADD R8,8
    buf.emit(&[0x83, 0xE9, 0x08]); // SUB ECX,8
    let back = jmp_patch(buf);
    patch_back(buf, back, qword_top);
    patch_here(buf, to_bytes);
    // (bit, load, store, advance by)
    let steps: [(u8, &[u8], &[u8], u8); 3] = [
        (4, &[0x8B, 0x02], &[0x41, 0x89, 0x00], 4), // MOV EAX,[RDX] / MOV [R8],EAX
        (2, &[0x0F, 0xB7, 0x02], &[0x66, 0x41, 0x89, 0x00], 2), // MOVZX EAX,WORD / MOV [R8],AX
        (1, &[0x0F, 0xB6, 0x02], &[0x41, 0x88, 0x00], 1), // MOVZX EAX,BYTE / MOV [R8],AL
    ];
    for (bit, load, store, n) in steps {
        buf.emit(&[0xF6, 0xC1, bit]); // TEST CL,bit
        let skip = jcc_patch(buf, 0x84); // JZ
        buf.emit(load);
        buf.emit(store);
        buf.emit(&[0x48, 0x83, 0xC2, n]); // ADD RDX,n
        buf.emit(&[0x49, 0x83, 0xC0, n]); // ADD R8,n
        patch_here(buf, skip);
    }
}

// ---- the UTF16 arm (STRGAP4-4) ----------------------------------------------------
//
// JIT round 19 proposal wave 7, lane str7 (`CRATONVM_JIT_CONCAT_UTF16`, default
// OFF). A wide plan's literal text holds a unit above 0xFF, so its result is
// always UTF16 -- the coder HotSpot's `StringConcatHelper` computes when it ORs
// the literal's coder in, and a char above 0xFF can never be compressed away.
// The result is `byte[2 * units]` holding little-endian UTF-16 units
// (`StringUTF16.isBigEndian() == false` on x86-64, the VM's own reader's order)
// and a String with `coder = UTF16`; the tails store that coder.
//
// Operands: a `String` is copied unit for unit (a LATIN1 payload inflated, a
// UTF16 payload copied), `null` is "null", `int`/`long` their decimal text,
// `char` its one unit (any value), `boolean` "true"/"false" -- the same
// spellings as the Latin-1 passes, two bytes per unit.

/// The UTF-16LE bytes of `units`.
fn utf16le(units: &[u16]) -> Vec<u8> {
    units.iter().flat_map(|u| u.to_le_bytes()).collect()
}

/// The UTF-16LE bytes of ASCII `text`.
fn ascii_utf16le(text: &[u8]) -> Vec<u8> {
    text.iter().flat_map(|&b| [b, 0]).collect()
}

/// [`emit_length_pass`] of a wide plan: R8 = the result's length in UTF-16
/// units, every operand screened; ECX = 2 * R8 (the `byte[]`'s length) on the
/// fall-through. Every refusal is pushed onto `decline` before anything is
/// stored. Clobbers RAX, RCX, RDX, R8, R11 and the flags.
fn emit_length_pass_wide(
    buf: &mut ExecutableBuffer,
    s: &ConcatPassShape,
    decline: &mut Vec<usize>,
) {
    // Compact strings off: the String layout this tail writes (`byte[]` +
    // coder) is the compact one; keep the bridge, as the Latin-1 pass does.
    // Cast: the flag's address, baked as an imm64.
    mov_imm64_full(buf, R11, s.compact_flag_addr as i64);
    cmp_mem8_imm8(buf, R11, 0, 0);
    decline.push(jcc_patch(buf, 0x84)); // JE -> bridge
                                        // Cast: bounded by CONCAT_INLINE_MAX_LITERAL.
    mov_imm32_sx(buf, R8, s.plan.literal_len as i32);
    for seg in &s.plan.segments {
        let ConcatSegment::Arg { index, kind } = *seg else {
            continue;
        };
        load_frame(buf, RAX, s.arg_offsets[index]);
        match kind {
            ConcatArgKind::String => {
                test_r64_r64(buf, RAX);
                let non_null = jcc_patch(buf, 0x85); // JNZ
                add_r64_imm8(buf, R8, 4); // "null"
                let next = jmp_patch(buf);
                patch_here(buf, non_null);
                // CONCAT21-4: a fused `valueOf`'s exact Integer.
                let mut boxed_next = None;
                if let Some(b) = s.boxed_int {
                    let not_box = emit_boxed_int_load(buf, s, b, Some(&mut *decline));
                    emit_signed_digits_len(buf);
                    boxed_next = Some(jmp_patch(buf));
                    patch_here(buf, not_box);
                }
                // RDX = value (a byte[]), ECX = coder (0 or 1), else declined.
                emit_string_value_coded(buf, s, Some(&mut *decline), true);
                // EAX = value.length >> coder: the operand's units.
                mov_r32_mem(buf, RAX, RDX, i32::from(s.len_d8));
                buf.emit(&[0xD3, 0xE8]); // SHR EAX,CL
                buf.emit(&[0x49, 0x01, 0xC0]); // ADD R8,RAX
                patch_here(buf, next);
                if let Some(j) = boxed_next {
                    patch_here(buf, j);
                }
            }
            ConcatArgKind::Int | ConcatArgKind::Long => {
                if kind == ConcatArgKind::Int {
                    buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX,EAX
                }
                buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
                let non_negative = jcc_patch(buf, 0x89); // JNS
                buf.emit(&[0x49, 0xFF, 0xC0]); // INC R8
                buf.emit(&[0x48, 0xF7, 0xD8]); // NEG RAX
                patch_here(buf, non_negative);
                emit_digit_count(buf, R8);
            }
            // One unit, whatever its value: the result is UTF16 anyway.
            ConcatArgKind::Char => buf.emit(&[0x49, 0xFF, 0xC0]), // INC R8
            ConcatArgKind::Boolean => emit_bool_len(buf),
            ConcatArgKind::BoxedInt => {
                emit_boxed_len(buf, s, s.boxed_layouts.int, kind, true, decline, None)
            }
            ConcatArgKind::BoxedLong => {
                emit_boxed_len(buf, s, s.boxed_layouts.long, kind, true, decline, None)
            }
            ConcatArgKind::BoxedChar => {
                emit_boxed_len(buf, s, s.boxed_layouts.char, kind, true, decline, None)
            }
            ConcatArgKind::BoxedBool => {
                emit_boxed_len(buf, s, s.boxed_layouts.bool, kind, true, decline, None)
            }
            // CONCAT21-3: the prologue's ASCII text, one unit per byte.
            ConcatArgKind::Float | ConcatArgKind::Double => emit_fp_add_len(buf, s, index),
        }
    }
    // A wide plan from the recipe always has literal text, so its result is
    // never empty. A CONCAT21-1 twin may have none (`s1 + s2`): an empty
    // result keeps the bridge, as the LATIN1 pass does.
    if s.plan.literal_len == 0 {
        buf.emit(&[0x4D, 0x85, 0xC0]); // TEST R8,R8
        decline.push(jcc_patch(buf, 0x84)); // JZ
    }
    // CMP R8,max/2 ; JA -> bridge (unsigned): 2 * R8 bytes must fit the cap.
    buf.emit(&[0x49, 0x81, 0xF8]);
    buf.emit(&(s.max_count / 2).to_le_bytes());
    decline.push(jcc_patch(buf, 0x87));
    buf.emit(&[0x44, 0x89, 0xC1]); // MOV ECX,R8D
    buf.emit(&[0x01, 0xC9]); // ADD ECX,ECX: bytes
}

/// [`emit_fill_pass`] of a wide plan: on entry R11 = the fresh `byte[]`
/// (exactly 2 * units bytes), R9 = the String. Writes every segment in recipe
/// order as UTF-16LE through the cursor R8. Clobbers RAX, RCX, RDX, R8, R10
/// and the flags.
fn emit_fill_pass_wide(buf: &mut ExecutableBuffer, s: &ConcatPassShape) {
    let data = i32::from(s.data_d8);
    let len_d8 = i32::from(s.len_d8);
    lea_r64_mem(buf, R8, R11, data);
    for seg in &s.plan.segments {
        match seg {
            ConcatSegment::WideLiteral(units) => emit_literal(buf, &utf16le(units)),
            // A wide plan carries none; see `emit_fill_pass`.
            ConcatSegment::Literal(_) => {
                buf.mark_codegen_unencodable("concat-latin1-literal-in-wide-plan");
            }
            ConcatSegment::Arg { index, kind } => {
                load_frame(buf, RAX, s.arg_offsets[*index]);
                match kind {
                    ConcatArgKind::String => {
                        test_r64_r64(buf, RAX);
                        let non_null = jcc_patch(buf, 0x85); // JNZ
                        emit_literal(buf, &ascii_utf16le(b"null"));
                        let to_next = jmp_patch(buf);
                        patch_here(buf, non_null);
                        // CONCAT21-4: a fused `valueOf`'s exact Integer.
                        let mut boxed_next = None;
                        if let Some(b) = s.boxed_int {
                            let not_box = emit_boxed_int_load(buf, s, b, None);
                            emit_decimal_wide(buf);
                            boxed_next = Some(jmp_patch(buf));
                            patch_here(buf, not_box);
                        }
                        emit_string_value_coded(buf, s, None, true);
                        buf.emit(&[0x85, 0xC9]); // TEST ECX,ECX
                        let utf16 = jcc_patch(buf, 0x85); // JNZ
                                                          // LATIN1: inflate value.length bytes to units.
                        mov_r32_mem(buf, RCX, RDX, len_d8);
                        lea_r64_mem(buf, RDX, RDX, data);
                        emit_inflate(buf);
                        let inflated = jmp_patch(buf);
                        patch_here(buf, utf16);
                        // UTF16: copy the whole units (an even byte count,
                        // which is what the length pass counted).
                        mov_r32_mem(buf, RCX, RDX, len_d8);
                        buf.emit(&[0x83, 0xE1, 0xFE]); // AND ECX,-2
                        lea_r64_mem(buf, RDX, RDX, data);
                        emit_copy(buf);
                        patch_here(buf, inflated);
                        patch_here(buf, to_next);
                        if let Some(j) = boxed_next {
                            patch_here(buf, j);
                        }
                    }
                    ConcatArgKind::Int | ConcatArgKind::Long => {
                        if *kind == ConcatArgKind::Int {
                            buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX,EAX
                        }
                        emit_decimal_wide(buf);
                    }
                    ConcatArgKind::Char => {
                        buf.emit(&[0x66, 0x41, 0x89, 0x00]); // MOV [R8],AX
                        add_r64_imm8(buf, R8, 2);
                    }
                    ConcatArgKind::Boolean => {
                        buf.emit(&[0x85, 0xC0]); // TEST EAX,EAX
                        let is_false = jcc_patch(buf, 0x84); // JZ
                        emit_literal(buf, &ascii_utf16le(b"true"));
                        let next = jmp_patch(buf);
                        patch_here(buf, is_false);
                        emit_literal(buf, &ascii_utf16le(b"false"));
                        patch_here(buf, next);
                    }
                    ConcatArgKind::BoxedInt => {
                        emit_boxed_fill(buf, s, s.boxed_layouts.int, *kind, true)
                    }
                    ConcatArgKind::BoxedLong => {
                        emit_boxed_fill(buf, s, s.boxed_layouts.long, *kind, true)
                    }
                    ConcatArgKind::BoxedChar => {
                        emit_boxed_fill(buf, s, s.boxed_layouts.char, *kind, true)
                    }
                    ConcatArgKind::BoxedBool => {
                        emit_boxed_fill(buf, s, s.boxed_layouts.bool, *kind, true)
                    }
                    // CONCAT21-3: inflate the prologue's text.
                    ConcatArgKind::Float | ConcatArgKind::Double => {
                        emit_fp_text(buf, s, *index);
                        emit_inflate(buf);
                    }
                }
            }
        }
    }
}

/// ECX Latin-1 bytes from [RDX] to ECX UTF-16LE units at [R8]; both pointers
/// end past the copy. Clobbers RAX, RCX, RDX and the flags.
fn emit_inflate(buf: &mut ExecutableBuffer) {
    let top = buf.pos();
    buf.emit(&[0x85, 0xC9]); // TEST ECX,ECX
    let done = jcc_patch(buf, 0x84); // JZ
    buf.emit(&[0x0F, 0xB6, 0x02]); // MOVZX EAX,BYTE [RDX]
    buf.emit(&[0x66, 0x41, 0x89, 0x00]); // MOV [R8],AX
    buf.emit(&[0x48, 0xFF, 0xC2]); // INC RDX
    buf.emit(&[0x49, 0x83, 0xC0, 0x02]); // ADD R8,2
    buf.emit(&[0xFF, 0xC9]); // DEC ECX
    let back = jmp_patch(buf);
    patch_back(buf, back, top);
    patch_here(buf, done);
}

/// [`emit_decimal`] in UTF-16LE units: the decimal text of the signed 64-bit
/// RAX at [R8], R8 past it. Clobbers RAX, RCX, RDX, R10 and the flags.
fn emit_decimal_wide(buf: &mut ExecutableBuffer) {
    buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
    let non_negative = jcc_patch(buf, 0x89); // JNS
    buf.emit(&[0x66, 0x41, 0xC7, 0x00, b'-', 0x00]); // MOV WORD [R8],'-'
    buf.emit(&[0x49, 0x83, 0xC0, 0x02]); // ADD R8,2
    buf.emit(&[0x48, 0xF7, 0xD8]); // NEG RAX (MIN_VALUE: 2^63 unsigned)
    patch_here(buf, non_negative);
    buf.emit(&[0x31, 0xC9]); // XOR ECX,ECX
    emit_digit_count(buf, RCX);
    buf.emit(&[0x4D, 0x8D, 0x04, 0x48]); // LEA R8,[R8+RCX*2]: past the digits
    buf.emit(&[0x4D, 0x89, 0xC2]); // MOV R10,R8: write backwards
    let top = buf.pos();
    buf.emit(&[0x48, 0x89, 0xC1]); // MOV RCX,RAX
                                   // Cast: the reciprocal's bits as an imm64.
    mov_imm64_full(buf, RDX, CONCAT_DIV10_MAGIC as i64);
    buf.emit(&[0x48, 0xF7, 0xE2]); // MUL RDX: RDX:RAX = x * magic
    buf.emit(&[0x48, 0xC1, 0xEA, 0x03]); // SHR RDX,3: q = x / 10
    buf.emit(&[0x48, 0x8D, 0x04, 0x92]); // LEA RAX,[RDX+RDX*4]
    buf.emit(&[0x48, 0x01, 0xC0]); // ADD RAX,RAX: 10q
    buf.emit(&[0x48, 0x29, 0xC1]); // SUB RCX,RAX: x % 10
    buf.emit(&[0x83, 0xC1, 0x30]); // ADD ECX,'0'
    buf.emit(&[0x49, 0x83, 0xEA, 0x02]); // SUB R10,2
    buf.emit(&[0x66, 0x41, 0x89, 0x0A]); // MOV [R10],CX
    buf.emit(&[0x48, 0x89, 0xD0]); // MOV RAX,RDX
    buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
    let back = jcc_patch(buf, 0x85); // JNZ top
    patch_back(buf, back, top);
}

// ---- the optimizing tier's allocation -------------------------------------------

/// Everything the optimizing tier's inline concat bakes for one site
/// (STRGAP4-1), gathered by [`concat_ir_site`] before any byte is emitted.
#[derive(Clone, Debug)]
pub(crate) struct ConcatIrSite {
    pub(crate) shape: ConcatPassShape,
    alloc: SbToStringPlan,
    gc: SbToStringGc,
    thread: ConcatThread,
    cursor_off: i32,
    end_off: i32,
    /// `JIT_G1_BARRIER`'s address (`helpers.g1_barrier_addr`).
    g1_table: usize,
}

/// Where the optimizing tier's inline concat reads the `JvmThread*`: the
/// `JIT_THREAD` mirror (one segment load), or the frame's cached slot where
/// the target has no mirror. The mirror first, as the IR tier's inline thin
/// lock reads it: the frame slot's prologue fetch is ERASED in a body that
/// publishes no root (`fetch_current_thread`), and reads 0 there, which would
/// decline every evaluation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConcatThread {
    /// `MOV R10, <seg>:[disp32]`.
    Tls(u32),
    /// `MOV R10, [rbp - off]`.
    Frame(i32),
}

/// The run-time facts of the optimizing tier's frame and helper table that
/// [`concat_ir_site`] screens (the IR lowerer's own fields).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ConcatIrFrame {
    /// `jit_thread_tls_disp()`, or 0 with no mirror.
    pub(crate) tls_disp: usize,
    /// `shadow_thread_slot_off` (0 / negative = none).
    pub(crate) thread_slot_off: i32,
    pub(crate) cursor_off: i32,
    pub(crate) end_off: i32,
    pub(crate) class_id_off: i32,
    pub(crate) g1_table: usize,
    pub(crate) region_bounds_addr: usize,
}

/// Every compile-time screen of the optimizing tier's inline concat at an
/// `Op::IndyBridge` site whose operands are staged at `arg_offsets`; `Err`
/// names the refusal (the site keeps its bridge call, byte for byte). The
/// single-pass screens (`Compiler::concat_inline_site`) plus the IR frame's
/// own: a thread source that is not a call ([`ConcatThread`]), TLAB offsets.
pub(crate) fn concat_ir_site(
    plan: Option<&'static JitConcatInlinePlan>,
    string_layout: Option<crate::StringFieldLayout>,
    compact_flag_addr: Option<usize>,
    arg_offsets: Vec<i32>,
    frame: ConcatIrFrame,
    boxed_layouts: ConcatBoxedLayouts,
) -> Result<ConcatIrSite, &'static str> {
    let plan = plan.ok_or("no plan (shape not admitted, or reader not wired)")?;
    if plan.wide && !crate::concat_inline_plan::concat_utf16_enabled() {
        return Err("UTF16 literal (wide plan) without CRATONVM_JIT_CONCAT_UTF16");
    }
    // CONCAT21-3: a float / double operand only under its switch.
    if plan.has_fp() && !crate::concat_inline_plan::concat_fp_inline_enabled() {
        return Err("float/double operand without CRATONVM_JIT_CONCAT_FP_INLINE");
    }
    if is_unary_boolean(plan) {
        return Err("unary boolean (`\"\" + b`): HotSpot answers the interned literal");
    }
    if !boxed_layouts.all_resolved_for(plan) {
        return Err("missing boxed wrapper layout");
    }
    if plan.arg_count != arg_offsets.len() {
        return Err("operand count disagrees with the staged block");
    }
    let outer = string_layout.ok_or("no String layout")?;
    if !outer.has_coder || outer.string_class_id == 0 {
        return Err("String layout without coder");
    }
    let alloc = super::op_invoke::string_alloc_plan(&outer)?;
    // REVIEW6-3: the same fail-closed coder-slot pin as the single-pass site.
    if alloc.utf16_coder != Some((outer.coder_compact_offset, outer.coder_compact_is_byte)) {
        return Err("coder slot not validated by the allocation plan");
    }
    let gc = super::op_invoke::sb_to_string_gc_arm(super::op_invoke::SbToStringGcInputs {
        zgc_registration: cratonvm_types::jit_tlab_registration_required(),
        zgc_start_bit_requested: super::objects::zgc_inline_start_bit_requested(),
        zgc_array_announce: super::zgc_inline_array_announce_enabled(),
        g1_table_live: super::g1_barrier_table_live(frame.g1_table),
        generational_bounds_live: super::region_bounds_are_live(frame.region_bounds_addr),
    })?;
    let thread = match u32::try_from(frame.tls_disp) {
        Ok(d) if d != 0 => ConcatThread::Tls(d),
        _ if frame.thread_slot_off > 0 => ConcatThread::Frame(frame.thread_slot_off),
        _ => return Err("no thread source that is not a call"),
    };
    if frame.cursor_off == frame.end_off {
        return Err("no TLAB offsets");
    }
    let compact_flag_addr = compact_flag_addr.ok_or("no compact-strings flag address")?;
    let mut shape = ConcatPassShape::new(
        plan,
        outer,
        &alloc,
        compact_flag_addr,
        arg_offsets,
        frame.class_id_off,
    );
    shape.boxed_layouts = boxed_layouts;
    Ok(ConcatIrSite {
        shape,
        alloc,
        gc,
        thread,
        cursor_off: frame.cursor_off,
        end_off: frame.end_off,
        g1_table: frame.g1_table,
    })
}

impl ConcatIrSite {
    /// CONCAT21-1 (`CRATONVM_JIT_CONCAT_CODER_PROMOTE`): the same site over
    /// `twin`, the plan's wide twin. Every screen [`concat_ir_site`] made
    /// holds for it: same operands and offsets, and the UTF16 coder slot the
    /// wide tail stores was validated for every admitted site.
    pub(crate) fn with_plan(&self, twin: &'static JitConcatInlinePlan) -> Self {
        let mut site = self.clone();
        site.shape.plan = twin;
        site
    }
}

/// CONCAT21-1: the wide twin both tiers promote `plan`'s run-time UTF16
/// operands to, when the switch is on, or `None` (the LATIN1 passes then
/// decline those operands to the bridge, as before).
pub(crate) fn coder_promotion_twin(
    plan: &'static JitConcatInlinePlan,
) -> Option<&'static JitConcatInlinePlan> {
    if !crate::concat_inline_plan::concat_coder_promote_enabled() {
        return None;
    }
    plan.promotion_twin()
}

/// Is `plan` the unary boolean concat `"" + b` (recipe `\1`, one `Z`
/// operand, no literal text)? HotSpot 25's `StringConcatFactory` serves a
/// one-argument recipe with `String.valueOf(boolean)`, which answers the
/// interned `"true"` / `"false"` LITERAL (`R19Review5UnaryConcat`
/// `unary-boolean-literal` 200000/200000, C2 and `-Xint`), not a fresh
/// String; every other unary shape is fresh there (`unary-string-identity`
/// 0/200000, `Integer`/`Long.toString`, `String.valueOf(char)`). The
/// optimizing tier's inline concat builds fresh Strings only, so it declines
/// this one shape to the bridge (open page
/// `r19-unary-indy-concat-returns-a-fresh-string-20261008-FIXED-20261009.md`): whatever the
/// bridge answers, the inline path never widens that gap.
pub(crate) fn is_unary_boolean(plan: &JitConcatInlinePlan) -> bool {
    plan.literal_len == 0
        && matches!(
            plan.segments.as_slice(),
            [ConcatSegment::Arg {
                kind: ConcatArgKind::Boolean,
                ..
            }]
        )
}

/// The optimizing tier's whole inline concat over the staged operands: the
/// length pass, one TLAB bump for the `byte[]` and the `String`, the
/// collector pre-checks, both headers, the fill pass and the commit. Every
/// refusal is pushed onto `decline` BEFORE the first store; the fall-through
/// leaves RAX = the fresh String. Clobbers RAX, RCX, RDX, R8, R9, R10, R11 and
/// the flags -- a subset of the bridge call's clobbers, which is what the
/// node already is to the register allocator.
///
/// The same allocation as the single-pass STR16-P1 tail
/// (`Compiler::emit_sb_to_string_tail` with a `fill`), with the thread read
/// through [`ConcatThread`]: the layout-replace guard, the bump, the
/// G1 same-region / ZGC start-bit pre-checks, the `byte[]` header, the
/// String's header and zeroed body, `String.value`, the fill, the cursor
/// commit LAST, then the two ZGC start bits.
pub(crate) fn emit_ir_tail(
    buf: &mut ExecutableBuffer,
    site: &ConcatIrSite,
    decline: &mut Vec<usize>,
) {
    emit_ir_tail_with(buf, site, decline, None);
}

/// CONCAT21-1: [`emit_ir_tail`] of a LATIN1 site whose UTF16 edges go to
/// `promote` ([`emit_length_pass_promoting`]); the caller lands them on
/// [`emit_ir_tail`] of [`ConcatIrSite::with_plan`] of the twin.
pub(crate) fn emit_ir_tail_promoting(
    buf: &mut ExecutableBuffer,
    site: &ConcatIrSite,
    decline: &mut Vec<usize>,
    promote: &mut Vec<usize>,
) {
    emit_ir_tail_with(buf, site, decline, Some(promote));
}

fn emit_ir_tail_with(
    buf: &mut ExecutableBuffer,
    site: &ConcatIrSite,
    decline: &mut Vec<usize>,
    promote: Option<&mut Vec<usize>>,
) {
    let plan = &site.alloc;
    match promote {
        Some(p) => emit_length_pass_promoting(buf, &site.shape, decline, p),
        None => emit_length_pass(buf, &site.shape, decline),
    }

    // ---- String layout-replace guard (RAX, R11 scratch) --------------------
    mov_imm64_full(buf, R11, plan.replace_guard.0 as i64); // Cast: baked address
    mov_r32_mem(buf, RAX, R11, 0);
    buf.emit_byte(0x3D); // CMP EAX, imm32
    buf.emit(&(plan.replace_guard.1 as i32).to_le_bytes()); // Cast: counter bits
    decline.push(jcc_patch(buf, 0x85)); // JNE

    // ---- thread, then ONE bump for both objects ----------------------------
    load_thread_r10(buf, site.thread);
    test_r64_r64(buf, R10);
    decline.push(jcc_patch(buf, 0x84)); // JZ no thread -> call
                                        // R11 = align8(cursor): the byte[].
    mov_r64_mem(buf, R11, R10, site.cursor_off);
    add_r64_imm8(buf, R11, 7);
    and_r64_imm8(buf, R11, -8);
    // R8 = align8(count).
    buf.emit(&[0x41, 0x89, 0xC8]); // MOV R8D,ECX
    add_r64_imm8(buf, R8, 7);
    and_r64_imm8(buf, R8, -8);
    // LEA R9,[R11+R8+DATA]: the String.
    buf.emit(&[0x4F, 0x8D, 0x4C, 0x03, plan.data_d8]);
    // RAX = R9 + total: the new cursor. CMP RAX,[R10+end] ; JA -> call.
    lea_r64_mem(buf, RAX, R9, plan.string_total);
    cmp_r64_mem(buf, RAX, R10, site.end_off);
    decline.push(jcc_patch(buf, 0x87));

    // ---- per-collector pre-checks (RAX, R8 scratch) -------------------------
    if site.gc.g1_same_region {
        emit_g1_same_region_check(buf, site.g1_table, decline);
    }
    // zr z9/bo: one read of the form for the pre-check and both stores.
    let short_bits = site.gc.zgc_start_bits && super::objects::zgc_jit_short_announce_enabled();
    if site.gc.zgc_start_bits {
        emit_zgc_start_bit_precheck_form(
            buf,
            site.cursor_off,
            site.end_off,
            &[R11, R9],
            decline,
            short_bits,
        );
    }
    // ---- no decline past this point: every store below is committed --------

    // byte[] header: class id 0, length, mark word, "no hash yet".
    mov_dword_mem_imm32(buf, R11, 0, 0);
    buf.emit(&[0x41, 0x89, 0x4B, plan.len_d8]); // MOV [R11+len],ECX
    mov_dword_mem_imm32(
        buf,
        R11,
        cratonvm_types::MARK_WORD_OFFSET as i32, // Cast: small header constant
        plan.byte_array_mark,
    );
    mov_dword_mem_imm32(
        buf,
        R11,
        cratonvm_types::IDENTITY_HASH_OFFSET as i32, // Cast: small header constant
        0,
    );
    // String header: class id and the compact mark word (NEUTRAL,
    // kind=Object, GC_FLAG_HEADER | GC_FLAG_COMPACT), as the inline `new`.
    // Cast: a ClassId fits in 32 bits.
    mov_dword_mem_imm32(
        buf,
        R9,
        site.shape.class_id_off,
        plan.string_class_id as i32,
    );
    let flags = cratonvm_types::GC_FLAG_COMPACT | cratonvm_types::GC_FLAG_HEADER;
    mov_dword_mem_imm32(
        buf,
        R9,
        cratonvm_types::MARK_WORD_OFFSET as i32, // Cast: small header constant
        i32::from(flags) << 24,
    );
    // String body: `coder` LATIN1 (0), `hash` 0, `hashIsZero` false, padding.
    let mut off = i32::try_from(cratonvm_types::COMPACT_HEADER_SIZE).unwrap_or(plan.string_total);
    while off < plan.string_total {
        mov_dword_mem_imm32(buf, R9, off, 0);
        off += 4;
    }
    // STRGAP4-4: a wide plan's `coder = UTF16` (1), over the zero just
    // written -- the single-pass tail's UTF16-arm store, byte for byte.
    // `concat_ir_site` admitted the site only with this slot validated.
    if site.shape.plan.wide {
        if let Some((coder_off, is_byte)) = plan.utf16_coder {
            emit_coder_utf16_store(buf, coder_off, is_byte);
        }
    }
    // String.value = the byte[].
    mov_mem_r64(buf, R9, R11, plan.value_off);

    emit_fill_pass(buf, &site.shape);

    // ---- commit LAST, then answer the String --------------------------------
    // The fill used R10 as scratch: the thread again (proven non-null above).
    load_thread_r10(buf, site.thread);
    // JIT r20 wave 13 (lane huntGC2): both ZGC start bits BEFORE the commit
    // under `CRATONVM_ZGC_JIT_START_BIT_BEFORE_COMMIT` (default ON), the
    // single-pass tail's order (`Compiler::emit_sb_to_string_tail`) and the
    // chars tails' below: a peer frozen between the commit and the `or`s owned
    // two objects outside its published tail and unregistered
    // (`r21-zgc-frozen-peer-in-flight-object-is-swept`). The stores clobber
    // RAX, RCX, RDX and R8 only, so the cursor is derived after them from R9.
    let bits_first =
        site.gc.zgc_start_bits && super::objects::zgc_start_bit_before_commit_enabled();
    if bits_first {
        emit_zgc_start_bit_store_prechecked_form(buf, site.cursor_off, R11, short_bits);
        emit_zgc_start_bit_store_prechecked_form(buf, site.cursor_off, R9, short_bits);
    }
    lea_r64_mem(buf, RAX, R9, plan.string_total);
    mov_mem_r64(buf, R10, RAX, site.cursor_off);
    if site.gc.zgc_start_bits && !bits_first {
        emit_zgc_start_bit_store_prechecked_form(buf, site.cursor_off, R11, short_bits);
        emit_zgc_start_bit_store_prechecked_form(buf, site.cursor_off, R9, short_bits);
    }
    mov_r64_r64(buf, RAX, R9);
    // CONCAT21-3: RSP back before the join (nothing without FP operands).
    emit_fp_restore(buf, &site.shape);
}

/// `coder = UTF16` (1) into the String in R9 at `coder_off`: the single-pass
/// tail's UTF16-arm store (`Compiler::emit_sb_to_string_tail`), byte for byte.
fn emit_coder_utf16_store(buf: &mut ExecutableBuffer, coder_off: i32, is_byte: bool) {
    if is_byte {
        // MOV BYTE [R9+disp32],1: REX.B C6 /0, modrm 10 000 001.
        buf.emit(&[0x41, 0xC6, 0x81]);
        buf.emit(&coder_off.to_le_bytes());
        buf.emit_byte(1);
    } else {
        mov_dword_mem_imm32(buf, R9, coder_off, 1);
    }
}

/// R10 = the `JvmThread*` from `thread`.
fn load_thread_r10(buf: &mut ExecutableBuffer, thread: ConcatThread) {
    match thread {
        ConcatThread::Tls(disp) => {
            buf.emit_byte(super::inline_rbp_tls_segment_prefix());
            buf.emit(&[0x4C, 0x8B, 0x14, 0x25]); // MOV R10, <seg>:[disp32]
            buf.emit(&disp.to_le_bytes());
        }
        ConcatThread::Frame(off) => load_frame(buf, R10, off),
    }
}

// ---- collector pre-checks (byte-for-byte the uncounted Compiler forms) ----------

/// `Compiler::emit_sb_g1_same_region_check` with no counter: `((R11 - base) ^
/// (R9 - base)) & region_mask == 0`, else a decline. Clobbers RAX, R8 and the
/// flags.
pub(crate) fn emit_g1_same_region_check(
    buf: &mut ExecutableBuffer,
    table: usize,
    decline: &mut Vec<usize>,
) {
    // Cast: the 'static `JIT_G1_BARRIER` table's address, baked.
    let table = table as i64;
    mov_imm64_full(buf, RAX, table);
    mov_r64_mem(buf, RAX, RAX, super::Compiler::G1B_ARENA_BASE);
    mov_r64_r64(buf, R8, R11);
    alu_r64_r64(buf, 0x29, R8, RAX); // SUB R8,RAX: A - base
    buf.emit(&[0x48, 0xF7, 0xD8]); // NEG RAX
    buf.emit(&[0x4C, 0x01, 0xC8]); // ADD RAX,R9: RAX = S - base
    alu_r64_r64(buf, 0x31, RAX, R8); // XOR RAX,R8
    mov_imm64_full(buf, R8, table);
    and_r64_mem(buf, RAX, R8, super::Compiler::G1B_REGION_MASK);
    decline.push(jcc_patch(buf, 0x85)); // JNZ -> call
}

/// `Compiler::emit_zgc_start_bit_precheck` with `count == false`: every
/// condition of the start-bit table's contract for each of `objs`, asked
/// before the first store. R10 = the thread. Clobbers RAX, R8 and the flags.
pub(crate) fn emit_zgc_start_bit_precheck(
    buf: &mut ExecutableBuffer,
    cursor_off: i32,
    end_off: i32,
    objs: &[u8],
    decline: &mut Vec<usize>,
) {
    let short = super::objects::zgc_jit_short_announce_enabled();
    // rx27 wave 3b, lane alloc (`CRATONVM_JIT_ZGC_LEAN_START_BIT`, opt-in).
    if !short && super::objects::zgc_lean_start_bit_enabled() {
        let lean = super::objects::LeanStartBit {
            short_gate: true,
            bts: super::objects::zgc_start_bit_bts_enabled(),
        };
        super::objects::note_lean_start_bit_site("pair", lean, true);
        emit_zgc_start_bit_precheck_lean(buf, cursor_off, objs, decline);
        return;
    }
    emit_zgc_start_bit_precheck_form(buf, cursor_off, end_off, objs, decline, short);
}

/// Round rx27 wave 3b, lane alloc ([`super::objects::zgc_lean_start_bit_enabled`]):
/// the long pre-check with the zr z11/ia + s1/ca trims the inline `new` store
/// has had since 2026-10-13. Shared: the armed table, the epoch, the
/// `short_blocked` gate (implies `blocked == 0`; closed on a misaligned base,
/// which only declines), `words != 0`. Per object: the word index
/// `(obj - base) >> 9` inside the buffer's owned-word window
/// (`Tlab::zgc_owned_words`), which is exactly the long form's two granule
/// tests. No span or grid test: every caller passes objects of ONE bump that
/// passed the TLAB end test (`start <= obj < end`), 8-aligned, and an armed
/// buffer lies inside `[base, base + span)` with a non-empty window only when
/// every index of the span fits the dword compared
/// (`gc::tlab::zgc_owned_word_window`) -- the argument of
/// `zgc_announce_skip_span_test_enabled` / `zgc_announce_skip_grid_test_enabled`.
/// R10 = the thread. Clobbers RAX, R8 and the flags (RAX = the table after it,
/// as before).
pub(crate) fn emit_zgc_start_bit_precheck_lean(
    buf: &mut ExecutableBuffer,
    cursor_off: i32,
    objs: &[u8],
    decline: &mut Vec<usize>,
) {
    use super::objects::{
        emit_cmp_qword_rax_disp_zero, ZGC_ANNOUNCE_BASE, ZGC_ANNOUNCE_EPOCH,
        ZGC_ANNOUNCE_SHORT_BLOCKED, ZGC_ANNOUNCE_WORDS, ZGC_TLAB_ANNOUNCE_OFFSET,
        ZGC_TLAB_CURSOR_OFFSET, ZGC_TLAB_OWNED_EPOCH_OFFSET, ZGC_TLAB_OWNED_WORDS_OFFSET,
    };
    debug_assert!(objs.iter().all(|r| *r != RAX && *r != R8 && *r != R10));
    let tlab = cursor_off - ZGC_TLAB_CURSOR_OFFSET;
    mov_r64_mem(buf, RAX, R10, tlab + ZGC_TLAB_ANNOUNCE_OFFSET);
    test_r64_r64(buf, RAX);
    decline.push(jcc_patch(buf, 0x84)); // JE: unarmed
    mov_r64_mem(buf, R8, RAX, ZGC_ANNOUNCE_EPOCH);
    cmp_r64_mem(buf, R8, R10, tlab + ZGC_TLAB_OWNED_EPOCH_OFFSET);
    decline.push(jcc_patch(buf, 0x85)); // JNE: stale ownership
    emit_cmp_qword_rax_disp_zero(buf, ZGC_ANNOUNCE_SHORT_BLOCKED);
    decline.push(jcc_patch(buf, 0x85)); // JNE: marking / generational / misaligned
    mov_r64_mem(buf, R8, RAX, ZGC_ANNOUNCE_WORDS);
    test_r64_r64(buf, R8);
    decline.push(jcc_patch(buf, 0x84)); // JE: unpublished
    for &obj in objs {
        mov_r64_r64(buf, R8, obj);
        sub_r64_mem(buf, R8, RAX, ZGC_ANNOUNCE_BASE);
        shr_r64_imm8(buf, R8, 9);
        // CMP R8D, [R10 + owned_words]; JB / CMP R8D, [R10 + owned_words + 4];
        // JAE: REX.R+B 3B /r, modrm 10 000 010.
        buf.emit(&[0x45, 0x3B, 0x82]);
        buf.emit(&(tlab + ZGC_TLAB_OWNED_WORDS_OFFSET).to_le_bytes());
        decline.push(jcc_patch(buf, 0x82));
        buf.emit(&[0x45, 0x3B, 0x82]);
        buf.emit(&(tlab + ZGC_TLAB_OWNED_WORDS_OFFSET + 4).to_le_bytes());
        decline.push(jcc_patch(buf, 0x83));
    }
}

/// [`emit_zgc_start_bit_precheck`] in the form `short` names (zr z9/bo,
/// `CRATONVM_ZGC_JIT_SHORT_ANNOUNCE`): the short form tests the armed table,
/// the epoch, `short_blocked`, then per object only the owned word
/// (`obj & -512 >= start`, `obj | 511 < end`). Byte for byte the uncounted
/// `Compiler::emit_zgc_start_bit_precheck_form`.
pub(crate) fn emit_zgc_start_bit_precheck_form(
    buf: &mut ExecutableBuffer,
    cursor_off: i32,
    end_off: i32,
    objs: &[u8],
    decline: &mut Vec<usize>,
    short: bool,
) {
    if short {
        use super::objects::{
            emit_cmp_qword_rax_disp_zero, ZGC_ANNOUNCE_EPOCH, ZGC_ANNOUNCE_SHORT_BLOCKED,
            ZGC_TLAB_ANNOUNCE_OFFSET, ZGC_TLAB_CURSOR_OFFSET, ZGC_TLAB_OWNED_EPOCH_OFFSET,
            ZGC_TLAB_START_OFFSET,
        };
        debug_assert!(objs.iter().all(|r| *r != RAX && *r != R8 && *r != R10));
        let tlab = cursor_off - ZGC_TLAB_CURSOR_OFFSET;
        mov_r64_mem(buf, RAX, R10, tlab + ZGC_TLAB_ANNOUNCE_OFFSET);
        test_r64_r64(buf, RAX);
        decline.push(jcc_patch(buf, 0x84)); // JE
        mov_r64_mem(buf, R8, RAX, ZGC_ANNOUNCE_EPOCH);
        cmp_r64_mem(buf, R8, R10, tlab + ZGC_TLAB_OWNED_EPOCH_OFFSET);
        decline.push(jcc_patch(buf, 0x85)); // JNE
        emit_cmp_qword_rax_disp_zero(buf, ZGC_ANNOUNCE_SHORT_BLOCKED);
        decline.push(jcc_patch(buf, 0x85)); // JNE
        for &obj in objs {
            mov_r64_r64(buf, R8, obj);
            buf.emit(&[0x49, 0x81, 0xE0]); // AND R8,imm32
            buf.emit(&(-512i32).to_le_bytes());
            cmp_r64_mem(buf, R8, R10, tlab + ZGC_TLAB_START_OFFSET);
            decline.push(jcc_patch(buf, 0x82)); // JB
            buf.emit(&[0x49, 0x81, 0xC8]); // OR R8,imm32
            buf.emit(&511i32.to_le_bytes());
            cmp_r64_mem(buf, R8, R10, end_off);
            decline.push(jcc_patch(buf, 0x83)); // JAE
        }
        return;
    }
    use super::objects::{
        ZGC_ANNOUNCE_BASE, ZGC_ANNOUNCE_BLOCKED, ZGC_ANNOUNCE_EPOCH, ZGC_ANNOUNCE_SPAN,
        ZGC_ANNOUNCE_WORDS, ZGC_TLAB_ANNOUNCE_OFFSET, ZGC_TLAB_CURSOR_OFFSET,
        ZGC_TLAB_OWNED_EPOCH_OFFSET, ZGC_TLAB_START_OFFSET,
    };
    debug_assert!(objs.iter().all(|r| *r != RAX && *r != R8 && *r != R10));
    let tlab = cursor_off - ZGC_TLAB_CURSOR_OFFSET;
    // 1. The buffer's armed table, or 0.
    mov_r64_mem(buf, RAX, R10, tlab + ZGC_TLAB_ANNOUNCE_OFFSET);
    test_r64_r64(buf, RAX);
    decline.push(jcc_patch(buf, 0x84)); // JE
                                        // 2. Ownership still valid.
    mov_r64_mem(buf, R8, RAX, ZGC_ANNOUNCE_EPOCH);
    cmp_r64_mem(buf, R8, R10, tlab + ZGC_TLAB_OWNED_EPOCH_OFFSET);
    decline.push(jcc_patch(buf, 0x85)); // JNE
                                        // 3. Not marking, not generational.
    mov_r64_mem(buf, R8, RAX, ZGC_ANNOUNCE_BLOCKED);
    test_r64_r64(buf, R8);
    decline.push(jcc_patch(buf, 0x85)); // JNE
                                        // 4a. A published bitmap.
    mov_r64_mem(buf, R8, RAX, ZGC_ANNOUNCE_WORDS);
    test_r64_r64(buf, R8);
    decline.push(jcc_patch(buf, 0x84)); // JE
    for &obj in objs {
        // 4b. R8 = obj - base, in range and on the 8-byte grid.
        mov_r64_r64(buf, R8, obj);
        sub_r64_mem(buf, R8, RAX, ZGC_ANNOUNCE_BASE);
        decline.push(jcc_patch(buf, 0x82)); // JB: obj < base
        cmp_r64_mem(buf, R8, RAX, ZGC_ANNOUNCE_SPAN);
        decline.push(jcc_patch(buf, 0x83)); // JAE: off >= span
        test_r64_imm32(buf, R8, 7);
        decline.push(jcc_patch(buf, 0x85)); // JNE: off-grid
                                            // 5. R8 = word_lo = base + (off & -512) ...
        buf.emit(&[0x49, 0x81, 0xE0]); // AND R8,imm32
        buf.emit(&(-512i32).to_le_bytes());
        buf.emit(&[0x4C, 0x03, 0x80]); // ADD R8,[RAX+disp32]
        buf.emit(&ZGC_ANNOUNCE_BASE.to_le_bytes());
        // ... >= start ...
        cmp_r64_mem(buf, R8, R10, tlab + ZGC_TLAB_START_OFFSET);
        decline.push(jcc_patch(buf, 0x82)); // JB
                                            // ... and word_lo + 512 <= end.
        buf.emit(&[0x49, 0x81, 0xC0]); // ADD R8,imm32
        buf.emit(&512i32.to_le_bytes());
        cmp_r64_mem(buf, R8, R10, end_off);
        decline.push(jcc_patch(buf, 0x87)); // JA
    }
}

/// `Compiler::emit_zgc_start_bit_store_prechecked`: the start bit of `obj`,
/// behind a passed [`emit_zgc_start_bit_precheck`] that named it with no poll
/// or call since, after the header and the commit. R10 = the thread.
/// Clobbers RAX, RCX, RDX, R8 and the flags.
pub(crate) fn emit_zgc_start_bit_store_prechecked(
    buf: &mut ExecutableBuffer,
    cursor_off: i32,
    obj: u8,
) {
    let short = super::objects::zgc_jit_short_announce_enabled();
    // rx27 wave 3b, lane alloc (`CRATONVM_JIT_ZGC_LEAN_START_BIT`, opt-in).
    if !short && super::objects::zgc_lean_start_bit_enabled() {
        let bts = super::objects::zgc_start_bit_bts_enabled();
        emit_zgc_start_bit_store_prechecked_lean(buf, cursor_off, obj, bts);
        return;
    }
    emit_zgc_start_bit_store_prechecked_form(buf, cursor_off, obj, short);
}

/// Round rx27 wave 3b, lane alloc: the long pre-checked store without the
/// `AND RCX, 63` (`SHL r64, CL` masks its count), and with `bts`
/// ([`super::objects::zgc_start_bit_bts_enabled`]) the bit set by one
/// `BTS QWORD [words], RCX`. Valid behind either pre-check form (both prove
/// the table armed and published and `obj`'s word owned), with no poll or
/// call since. Same word, same bit, same unlocked RMW. Clobbers RAX, RCX,
/// RDX, R8 and the flags, as the default form.
pub(crate) fn emit_zgc_start_bit_store_prechecked_lean(
    buf: &mut ExecutableBuffer,
    cursor_off: i32,
    obj: u8,
    bts: bool,
) {
    use super::objects::{
        BTS_QWORD_RDX_RCX, ZGC_ANNOUNCE_BASE, ZGC_ANNOUNCE_WORDS, ZGC_TLAB_ANNOUNCE_OFFSET,
        ZGC_TLAB_CURSOR_OFFSET,
    };
    debug_assert!(![RAX, RCX, RDX, R8, R10].contains(&obj));
    let tlab = cursor_off - ZGC_TLAB_CURSOR_OFFSET;
    mov_r64_mem(buf, RAX, R10, tlab + ZGC_TLAB_ANNOUNCE_OFFSET);
    mov_r64_mem(buf, RDX, RAX, ZGC_ANNOUNCE_WORDS);
    mov_r64_r64(buf, RCX, obj);
    sub_r64_mem(buf, RCX, RAX, ZGC_ANNOUNCE_BASE);
    shr_r64_imm8(buf, RCX, 3);
    if bts {
        buf.emit(&BTS_QWORD_RDX_RCX);
        return;
    }
    mov_r64_r64(buf, R8, RCX);
    shr_r64_imm8(buf, R8, 6);
    mov_imm32_sx(buf, RAX, 1);
    buf.emit(&[0x48, 0xD3, 0xE0]); // SHL RAX,CL
    buf.emit(&[0x4A, 0x09, 0x04, 0xC2]); // OR [RDX+R8*8],RAX
}

/// [`emit_zgc_start_bit_store_prechecked`] in the form `short` names (zr
/// z9/bo): behind a SHORT pre-check, the word through `granule_bias`. Byte
/// for byte `Compiler::emit_zgc_start_bit_store_prechecked_form`.
pub(crate) fn emit_zgc_start_bit_store_prechecked_form(
    buf: &mut ExecutableBuffer,
    cursor_off: i32,
    obj: u8,
    short: bool,
) {
    if short {
        use super::objects::{
            ZGC_ANNOUNCE_GRANULE_BIAS, ZGC_TLAB_ANNOUNCE_OFFSET, ZGC_TLAB_CURSOR_OFFSET,
        };
        debug_assert!(![RAX, RCX, RDX, R8, R10].contains(&obj));
        let tlab = cursor_off - ZGC_TLAB_CURSOR_OFFSET;
        mov_r64_mem(buf, RAX, R10, tlab + ZGC_TLAB_ANNOUNCE_OFFSET);
        mov_r64_mem(buf, RDX, RAX, ZGC_ANNOUNCE_GRANULE_BIAS);
        mov_r64_r64(buf, R8, obj);
        shr_r64_imm8(buf, R8, 9);
        mov_r64_r64(buf, RCX, obj);
        shr_r64_imm8(buf, RCX, 3);
        mov_imm32_sx(buf, RAX, 1);
        buf.emit(&[0x48, 0xD3, 0xE0]); // SHL RAX,CL
        buf.emit(&[0x4A, 0x09, 0x04, 0xC2]); // OR [RDX+R8*8],RAX
        return;
    }
    use super::objects::{
        ZGC_ANNOUNCE_BASE, ZGC_ANNOUNCE_WORDS, ZGC_TLAB_ANNOUNCE_OFFSET, ZGC_TLAB_CURSOR_OFFSET,
    };
    debug_assert!(![RAX, RCX, RDX, R8, R10].contains(&obj));
    let tlab = cursor_off - ZGC_TLAB_CURSOR_OFFSET;
    mov_r64_mem(buf, RAX, R10, tlab + ZGC_TLAB_ANNOUNCE_OFFSET);
    mov_r64_mem(buf, RDX, RAX, ZGC_ANNOUNCE_WORDS);
    mov_r64_r64(buf, RCX, obj);
    sub_r64_mem(buf, RCX, RAX, ZGC_ANNOUNCE_BASE);
    shr_r64_imm8(buf, RCX, 3);
    mov_r64_r64(buf, R8, RCX);
    shr_r64_imm8(buf, R8, 6);
    and_r64_imm8(buf, RCX, 63);
    mov_imm32_sx(buf, RAX, 1);
    buf.emit(&[0x48, 0xD3, 0xE0]); // SHL RAX,CL
    buf.emit(&[0x4A, 0x09, 0x04, 0xC2]); // OR [RDX+R8*8],RAX
}

// ---- the `char[]` copies (JIT r20 wave 12, lane qStr, W6-L2-2) ------------------
//
// `String.toCharArray()`, `String.valueOf(char[])` / `copyValueOf(char[])` and
// `new String(char[])` as allocating prefixes of the optimizing tier's
// `Op::Call` (`ir_lower::Lowerer::try_emit_ir_string_chars_prefix`, behind
// `CRATONVM_JIT_IR_STRING_CHARS_PREFIX`). Before them the call crossed into a
// native (`--compatible`: `String.<init>([C)V`, `toCharArray()[C`) or ran the
// JDK's chain (`--real-jdk`: `StringUTF16.compress` one char per iteration, or
// `isLatin1()` + `StringLatin1.toChars` + `inflate`). The allocation is the
// inline concat's ([`emit_ir_tail`]): the same thread source, the same
// collector arm ([`super::op_invoke::sb_to_string_gc_arm`]) and pre-checks, the
// same header stores, the commit LAST; every decline precedes the first store
// and lands on the unchanged call, so a declined evaluation repeats nothing.

/// Everything the optimizing tier's `char[]` copy prefixes bake for one site,
/// gathered by [`chars_ir_site`] before any byte is emitted.
#[derive(Clone, Debug)]
pub(crate) struct CharsIrSite {
    alloc: SbToStringPlan,
    gc: SbToStringGc,
    thread: ConcatThread,
    cursor_off: i32,
    end_off: i32,
    /// `JIT_G1_BARRIER`'s address (`helpers.g1_barrier_addr`).
    g1_table: usize,
    /// `helpers.class_id_offset_in_obj`.
    class_id_off: i32,
    /// This VM's `compact_strings` flag byte.
    compact_flag_addr: usize,
    /// The Generational backend's `JIT_REGION_BOUNDS` (two young semispaces
    /// first) when it is published, else 0: the `<init>` arm's receiver is
    /// not fresh, so on that backend it must be proven young.
    young_bounds_addr: usize,
    /// `String.coder`'s compact `(offset, is_byte)`, validated by the plan.
    coder: (i32, bool),
    /// `KIND_TAGS_BYTE_OFFSET` as a disp8, and the `byte[]` / `char[]` tags.
    kind_d8: u8,
    byte_array_tag: u8,
    char_array_tag: u8,
    /// The mark word of a fresh `char[]` (`ObjectHeader::new`'s own bits).
    char_array_mark: i32,
}

/// Every compile-time screen of the `char[]` copy prefixes: the inline
/// concat's ([`concat_ir_site`]) minus the plan, plus the `char[]` header.
/// `Err` names the refusal; the site then keeps its call, byte for byte.
pub(crate) fn chars_ir_site(
    string_layout: Option<crate::StringFieldLayout>,
    compact_flag_addr: Option<usize>,
    frame: ConcatIrFrame,
) -> Result<CharsIrSite, &'static str> {
    use cratonvm_types::{ArrayElementType, ClassId, ObjectHeader, ObjectKind};
    let outer = string_layout.ok_or("no String layout")?;
    if !outer.has_coder || outer.string_class_id == 0 {
        return Err("String layout without coder");
    }
    let alloc = super::op_invoke::string_alloc_plan(&outer)?;
    // REVIEW6-3's fail-closed coder-slot pin, as the inline concat.
    let coder = match alloc.utf16_coder {
        Some(c) if c == (outer.coder_compact_offset, outer.coder_compact_is_byte) => c,
        _ => return Err("coder slot not validated by the allocation plan"),
    };
    let gc = super::op_invoke::sb_to_string_gc_arm(super::op_invoke::SbToStringGcInputs {
        zgc_registration: cratonvm_types::jit_tlab_registration_required(),
        zgc_start_bit_requested: super::objects::zgc_inline_start_bit_requested(),
        zgc_array_announce: super::zgc_inline_array_announce_enabled(),
        g1_table_live: super::g1_barrier_table_live(frame.g1_table),
        generational_bounds_live: super::region_bounds_are_live(frame.region_bounds_addr),
    })?;
    let thread = match u32::try_from(frame.tls_disp) {
        Ok(d) if d != 0 => ConcatThread::Tls(d),
        _ if frame.thread_slot_off > 0 => ConcatThread::Frame(frame.thread_slot_off),
        _ => return Err("no thread source that is not a call"),
    };
    if frame.cursor_off == frame.end_off {
        return Err("no TLAB offsets");
    }
    let compact_flag_addr = compact_flag_addr.ok_or("no compact-strings flag address")?;
    let kind_d8 = u8::try_from(cratonvm_types::KIND_TAGS_BYTE_OFFSET)
        .ok()
        .filter(|d| *d <= 127)
        .ok_or("kind tags offset past disp8")?;
    let byte_array_tag =
        cratonvm_types::primitive_array_kind_tags_byte("[B").ok_or("no byte[] kind tag")?;
    let char_array_tag =
        cratonvm_types::primitive_array_kind_tags_byte("[C").ok_or("no char[] kind tag")?;
    let char_array_mark = ObjectHeader::new(
        ClassId::new(0),
        ObjectKind::Array,
        ArrayElementType::Char,
        0,
        0,
    )
    .mark_word
    .load(std::sync::atomic::Ordering::Relaxed);
    let young_bounds_addr = if super::region_bounds_are_live(frame.region_bounds_addr) {
        frame.region_bounds_addr
    } else {
        0
    };
    Ok(CharsIrSite {
        alloc,
        gc,
        thread,
        cursor_off: frame.cursor_off,
        end_off: frame.end_off,
        g1_table: frame.g1_table,
        class_id_off: frame.class_id_off,
        compact_flag_addr,
        young_bounds_addr,
        coder,
        kind_d8,
        byte_array_tag,
        char_array_tag,
        // Cast: the mark word's bits as an imm32.
        char_array_mark: char_array_mark as i32,
    })
}

/// `String.toCharArray()` inline (W6-L2-2). On entry RDX = the receiver's
/// `value` (the caller proved the receiver a non-null `java/lang/String` and
/// read the field by its own layout) and ECX = its `coder`. A null or
/// non-`byte[]` value, a coder other than 0 / 1, an odd UTF16 byte length, a
/// result over the inline cap, no thread, a full TLAB or a ZGC pre-check miss
/// is pushed onto `decline` before anything is stored. Otherwise a fresh
/// `char[length()]` is bumped, its header written, the payload inflated
/// (LATIN1) or copied (UTF16: the `byte[]` already holds the little-endian
/// units a `char[]` holds), the cursor committed LAST, and RAX = the array
/// -- `StringLatin1.toChars` / `StringUTF16.toChars`' answer, a fresh array
/// even for `""`. A primitive array: no reference store, so no barrier.
/// Clobbers RAX, RCX, RDX, R8, R9, R10, R11, XMM0, XMM1 and the flags.
pub(crate) fn emit_ir_to_char_array(
    buf: &mut ExecutableBuffer,
    site: &CharsIrSite,
    decline: &mut Vec<usize>,
) {
    let plan = &site.alloc;
    let data = i32::from(plan.data_d8);
    let len_d8 = plan.len_d8;
    // ---- the value and the length in chars ---------------------------------
    test_r64_r64(buf, RDX);
    decline.push(jcc_patch(buf, 0x84)); // JZ: no value
    cmp_mem8_imm8(buf, RDX, i32::from(site.kind_d8), site.byte_array_tag);
    decline.push(jcc_patch(buf, 0x85)); // JNE: not a byte[]
    buf.emit(&[0x41, 0x89, 0xC9]); // MOV R9D,ECX: the coder
    buf.emit(&[0x41, 0x83, 0xF9, 0x01]); // CMP R9D,1
    decline.push(jcc_patch(buf, 0x87)); // JA: no such coder
    buf.emit(&[0x8B, 0x4A, len_d8]); // MOV ECX,[RDX+len]: the byte length
    buf.emit(&[0x45, 0x85, 0xC9]); // TEST R9D,R9D
    let latin1 = jcc_patch(buf, 0x84); // JZ: one char per byte
    buf.emit(&[0xF6, 0xC1, 0x01]); // TEST CL,1
    decline.push(jcc_patch(buf, 0x85)); // JNZ: an odd UTF16 length
    buf.emit(&[0xD1, 0xE9]); // SHR ECX,1
    patch_here(buf, latin1);
    // ECX = length(): the char[]'s length, 2 * ECX payload bytes.
    buf.emit(&[0x81, 0xF9]); // CMP ECX,imm32
    buf.emit(&(plan.max_count / 2).to_le_bytes());
    decline.push(jcc_patch(buf, 0x87)); // JA: past the inline cap

    // ---- thread, then the bump ----------------------------------------------
    load_thread_r10(buf, site.thread);
    test_r64_r64(buf, R10);
    decline.push(jcc_patch(buf, 0x84)); // JZ no thread -> call
    mov_r64_mem(buf, R11, R10, site.cursor_off); // R11 = align8(cursor): the char[]
    add_r64_imm8(buf, R11, 7);
    and_r64_imm8(buf, R11, -8);
    buf.emit(&[0x4C, 0x8D, 0x04, 0x09]); // LEA R8,[RCX+RCX]: payload bytes
    add_r64_imm8(buf, R8, 7);
    and_r64_imm8(buf, R8, -8);
    buf.emit(&[0x4B, 0x8D, 0x44, 0x03, plan.data_d8]); // LEA RAX,[R11+R8+DATA]
    cmp_r64_mem(buf, RAX, R10, site.end_off);
    decline.push(jcc_patch(buf, 0x87)); // JA: TLAB full
    if site.gc.zgc_start_bits {
        // RAX / R8 die here; RCX, RDX, R9, R10, R11 survive.
        emit_zgc_start_bit_precheck(buf, site.cursor_off, site.end_off, &[R11], decline);
    }
    // ---- no decline past this point -----------------------------------------

    // char[] header: class id 0, length, mark word, "no hash yet".
    mov_dword_mem_imm32(buf, R11, 0, 0);
    buf.emit(&[0x41, 0x89, 0x4B, len_d8]); // MOV [R11+len],ECX
    mov_dword_mem_imm32(
        buf,
        R11,
        cratonvm_types::MARK_WORD_OFFSET as i32, // Cast: small header constant
        site.char_array_mark,
    );
    mov_dword_mem_imm32(
        buf,
        R11,
        cratonvm_types::IDENTITY_HASH_OFFSET as i32, // Cast: small header constant
        0,
    );
    // RDX = the source payload, R8 = the destination's.
    lea_r64_mem(buf, RDX, RDX, data);
    lea_r64_mem(buf, R8, R11, data);
    buf.emit(&[0x45, 0x85, 0xC9]); // TEST R9D,R9D
    let utf16 = jcc_patch(buf, 0x85); // JNZ
    emit_chars_inflate(buf);
    let copied = jmp_patch(buf);
    patch_here(buf, utf16);
    buf.emit(&[0x01, 0xC9]); // ADD ECX,ECX: the UTF16 payload's bytes
    emit_copy(buf);
    patch_here(buf, copied);

    // ---- commit LAST, then answer the array ---------------------------------
    // Under ZGC the start bit goes in BEFORE the commit when
    // `CRATONVM_ZGC_JIT_START_BIT_BEFORE_COMMIT` is on (the single-pass
    // tail's order: the array is fully written and no poll ran since the
    // pre-check); the store clobbers RAX, RCX, RDX and R8, so the new cursor
    // is derived after it, from the header.
    load_thread_r10(buf, site.thread);
    let bits_first =
        site.gc.zgc_start_bits && super::objects::zgc_start_bit_before_commit_enabled();
    if bits_first {
        emit_zgc_start_bit_store_prechecked(buf, site.cursor_off, R11);
    }
    buf.emit(&[0x41, 0x8B, 0x4B, len_d8]); // MOV ECX,[R11+len]
    buf.emit(&[0x48, 0x8D, 0x04, 0x09]); // LEA RAX,[RCX+RCX]
    add_r64_imm8(buf, RAX, 7);
    and_r64_imm8(buf, RAX, -8);
    buf.emit(&[0x49, 0x8D, 0x44, 0x03, plan.data_d8]); // LEA RAX,[R11+RAX+DATA]
    mov_mem_r64(buf, R10, RAX, site.cursor_off);
    if site.gc.zgc_start_bits && !bits_first {
        emit_zgc_start_bit_store_prechecked(buf, site.cursor_off, R11);
    }
    mov_r64_r64(buf, RAX, R11);
}

/// `Character.highSurrogate`'s bias, `MIN_HIGH_SURROGATE - (MIN_SUPPLEMENTARY_CODE_POINT
/// >>> 10)`: the high surrogate of a supplementary `cp` is `(cp >>> 10) + this`
/// (JIT r20 wave 19, lane zCp; shared with the `appendCodePoint` IR prefix).
pub(crate) const HIGH_SURROGATE_BIAS: i32 = 0xD800 - (0x1_0000 >> 10);
/// `Character.MIN_LOW_SURROGATE`: the low surrogate is `(cp & 0x3FF) + this`.
pub(crate) const LOW_SURROGATE_BASE: i32 = 0xDC00;

/// Can [`emit_ir_character_to_chars`] address `site`'s second payload char
/// with a disp8? Asked before any byte is emitted.
pub(crate) fn character_to_chars_site_fits(site: &CharsIrSite) -> bool {
    site.alloc.data_d8 <= 125 && site.alloc.len_d8 <= 127
}

/// `Character.toChars(int)` inline (JIT r20 wave 19, lane zCp,
/// `CRATONVM_JIT_IR_CHARACTER_TO_CHARS_PREFIX`). On entry R9D = the code
/// point. JDK 25's body:
///
/// ```text
/// if (isBmpCodePoint(cp))   return new char[] { (char) cp };   // cp >>> 16 == 0
/// if (isValidCodePoint(cp)) { char[] r = new char[2]; toSurrogates(cp, r, 0); return r; }
/// throw new IllegalArgumentException(String.format("Not a valid Unicode code point: 0x%X", cp));
/// ```
///
/// An invalid code point (negative, or past `0x10FFFF`), no thread, a full
/// TLAB or a ZGC pre-check miss is pushed onto `decline` before anything is
/// stored -- the call answers (and throws) exactly as before. Otherwise a
/// fresh `char[1]` / `char[2]` is bumped (header, then the one unit or the
/// high and low surrogates), the cursor committed LAST, and RAX = the array.
/// Both lengths take one 8-byte payload word after the header (the
/// [`emit_ir_to_char_array`] rounding of `2 * len`). A primitive array: no
/// reference store, so no barrier. Clobbers RAX, RCX, RDX, R8, R10, R11 and
/// the flags; R9 is read only.
pub(crate) fn emit_ir_character_to_chars(
    buf: &mut ExecutableBuffer,
    site: &CharsIrSite,
    decline: &mut Vec<usize>,
) {
    let plan = &site.alloc;
    let data = i32::from(plan.data_d8);
    let len_d8 = plan.len_d8;
    // The object: header + one 8-byte payload word (2 or 4 bytes used).
    let size = data + 8;

    // ---- ECX = the length in chars: 1 (BMP) or 2 (supplementary) -----------
    buf.emit(&[0x44, 0x89, 0xC8]); // MOV EAX,R9D
    buf.emit(&[0xC1, 0xE8, 0x10]); // SHR EAX,16
    mov_imm32_sx(buf, RCX, 1); // flags untouched
    let bmp = jcc_patch(buf, 0x84); // JZ: cp >>> 16 == 0
    buf.emit(&[0x41, 0x8D, 0x81]); // LEA EAX,[R9+disp32]
    buf.emit(&(-0x1_0000i32).to_le_bytes());
    buf.emit(&[0x3D]); // CMP EAX,imm32
    buf.emit(&0x10_0000i32.to_le_bytes());
    decline.push(jcc_patch(buf, 0x83)); // JAE: not a valid code point -> the call throws
    mov_imm32_sx(buf, RCX, 2);
    patch_here(buf, bmp);

    // ---- thread, then the bump ----------------------------------------------
    load_thread_r10(buf, site.thread);
    test_r64_r64(buf, R10);
    decline.push(jcc_patch(buf, 0x84)); // JZ no thread -> call
    mov_r64_mem(buf, R11, R10, site.cursor_off); // R11 = align8(cursor): the char[]
    add_r64_imm8(buf, R11, 7);
    and_r64_imm8(buf, R11, -8);
    lea_r64_mem(buf, RAX, R11, size); // RAX = the new cursor
    cmp_r64_mem(buf, RAX, R10, site.end_off);
    decline.push(jcc_patch(buf, 0x87)); // JA: TLAB full
    if site.gc.zgc_start_bits {
        // RAX / R8 die here; RCX, R9, R10, R11 survive.
        emit_zgc_start_bit_precheck(buf, site.cursor_off, site.end_off, &[R11], decline);
    }
    // ---- no decline past this point -----------------------------------------

    // char[] header: class id 0, length, mark word, "no hash yet".
    mov_dword_mem_imm32(buf, R11, 0, 0);
    buf.emit(&[0x41, 0x89, 0x4B, len_d8]); // MOV [R11+len],ECX
    mov_dword_mem_imm32(
        buf,
        R11,
        cratonvm_types::MARK_WORD_OFFSET as i32, // Cast: small header constant
        site.char_array_mark,
    );
    mov_dword_mem_imm32(
        buf,
        R11,
        cratonvm_types::IDENTITY_HASH_OFFSET as i32, // Cast: small header constant
        0,
    );
    // The payload.
    buf.emit(&[0x83, 0xF9, 0x01]); // CMP ECX,1
    let pair = jcc_patch(buf, 0x85); // JNE
    buf.emit(&[0x66, 0x45, 0x89, 0x4B, plan.data_d8]); // MOV WORD [R11+data],R9W
    let stored = jmp_patch(buf);
    patch_here(buf, pair);
    buf.emit(&[0x44, 0x89, 0xC8]); // MOV EAX,R9D
    buf.emit(&[0xC1, 0xE8, 0x0A]); // SHR EAX,10
    buf.emit(&[0x05]); // ADD EAX,imm32
    buf.emit(&HIGH_SURROGATE_BIAS.to_le_bytes());
    buf.emit(&[0x66, 0x41, 0x89, 0x43, plan.data_d8]); // MOV WORD [R11+data],AX
    buf.emit(&[0x44, 0x89, 0xC8]); // MOV EAX,R9D
    buf.emit(&[0x25]); // AND EAX,imm32
    buf.emit(&0x3FFi32.to_le_bytes());
    buf.emit(&[0x05]); // ADD EAX,imm32
    buf.emit(&LOW_SURROGATE_BASE.to_le_bytes());
    // `character_to_chars_site_fits` proved `data_d8 + 2` a disp8.
    buf.emit(&[0x66, 0x41, 0x89, 0x43, plan.data_d8 + 2]); // MOV WORD [R11+data+2],AX
    patch_here(buf, stored);

    // ---- commit LAST, then answer the array ---------------------------------
    // As `emit_ir_to_char_array`: under ZGC the start bit may go in before the
    // commit; its store clobbers RAX, RCX, RDX and R8, so the new cursor is
    // derived after it, from R11.
    load_thread_r10(buf, site.thread);
    let bits_first =
        site.gc.zgc_start_bits && super::objects::zgc_start_bit_before_commit_enabled();
    if bits_first {
        emit_zgc_start_bit_store_prechecked(buf, site.cursor_off, R11);
    }
    lea_r64_mem(buf, RAX, R11, size);
    mov_mem_r64(buf, R10, RAX, site.cursor_off);
    if site.gc.zgc_start_bits && !bits_first {
        emit_zgc_start_bit_store_prechecked(buf, site.cursor_off, R11);
    }
    mov_r64_r64(buf, RAX, R11);
}

/// `String.valueOf(char[])` / `copyValueOf(char[])` (`init == false`) and
/// `new String(char[])` at its `invokespecial` (`init == true`) inline
/// (W6-L2-2). On entry RDX = the `char[]` argument and, for `init`, R9 = the
/// receiver; either may be null.
///
/// The coder is JDK 25's: `String(char[], int, int, Void)` with
/// `COMPACT_STRINGS` takes `StringUTF16.compress(value, 0, len)` -- a LATIN1
/// `byte[len]` when every char is at most `0xFF`, else the UTF16
/// `byte[2 * len]` of `toBytes` -- and `coderFromArrayLen` names it by the
/// array's length; without `COMPACT_STRINGS` it is always UTF16. This scans
/// for a char above `0xFF` (four per step) when the VM's flag byte is set,
/// sizes the `byte[]` from the answer, and re-derives the coder from that
/// length after the collector pre-checks (which clobber every spare
/// register). The LATIN1 payload is the `PACKUSWB` compress (exact: every
/// unit fits), the UTF16 one a copy (the VM stores UTF16 little-endian, as a
/// `char[]` is).
///
/// Declines (to the call, nothing stored): the String layout replaced, a null
/// or non-`char[]` argument, an EMPTY array (the JDK answers `"".value`, a
/// shared array this body cannot name), a result over the inline cap, no
/// thread, a full TLAB, a collector pre-check miss; for `init`, a receiver
/// that is null, not exactly `java/lang/String`, legacy-layout, or whose
/// `value` is already set (a constructed String). The collector proofs are
/// the inline concat's for the one reference store `String.value = the
/// byte[]` (its old value is null: fresh, or tested for `init`): Generational
/// needs a young receiver (fresh for `valueOf`; TESTED against the published
/// young semispaces for `init`, which may have been promoted by a collection
/// between its `new` and this call), G1 the same-region test, ZGC the start-bit
/// pre-check (whose `blocked == 0` also means no mark and no generational
/// mode, so the store owes nothing whatever the receiver's age).
///
/// Each char is read ONCE for the payload, and the payload judges itself
/// (JIT r20 rev12, `jr20-string-from-chars-coder-scan-races-the-copy`): the
/// LATIN1 compress checks every unit it reads, and a UTF16 copy under
/// `COMPACT_STRINGS` is scanned in the private copy. A payload that disagrees
/// with the sizing scan (another thread wrote the array in between) commits
/// the allocation as unreachable garbage and takes the call, so no String is
/// ever saturated or non-canonical, as JDK 25's `StringUTF16.compress`.
///
/// Leaves RAX = the fresh String for `!init`. Clobbers RAX, RCX, RDX, R8,
/// R9, R10, R11, XMM0 and the flags.
pub(crate) fn emit_ir_string_from_chars(
    buf: &mut ExecutableBuffer,
    site: &CharsIrSite,
    decline: &mut Vec<usize>,
    init: bool,
) {
    let plan = &site.alloc;
    let data = i32::from(plan.data_d8);
    let len_d8 = plan.len_d8;
    // ---- String layout-replace guard (RAX, R11 scratch) --------------------
    mov_imm64_full(buf, R11, plan.replace_guard.0 as i64); // Cast: baked address
    mov_r32_mem(buf, RAX, R11, 0);
    buf.emit_byte(0x3D); // CMP EAX, imm32
    buf.emit(&(plan.replace_guard.1 as i32).to_le_bytes()); // Cast: counter bits
    decline.push(jcc_patch(buf, 0x85)); // JNE

    // ---- the argument (and the receiver) -----------------------------------
    test_r64_r64(buf, RDX);
    decline.push(jcc_patch(buf, 0x84)); // JZ: the call's NPE
    cmp_mem8_imm8(buf, RDX, i32::from(site.kind_d8), site.char_array_tag);
    decline.push(jcc_patch(buf, 0x85)); // JNE: not a char[]
    if init {
        test_r64_r64(buf, R9);
        decline.push(jcc_patch(buf, 0x84)); // JZ
                                            // CMP DWORD [R9+class_id],String (81 /7, REX.B).
        buf.emit(&[0x41, 0x81, 0xB9]);
        buf.emit(&site.class_id_off.to_le_bytes());
        // Cast: a ClassId fits in 32 bits.
        buf.emit(&(plan.string_class_id as i32).to_le_bytes());
        decline.push(jcc_patch(buf, 0x85)); // JNE
        test_mem8_imm8(
            buf,
            R9,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32, // Cast: small header constant
            cratonvm_types::GC_FLAG_COMPACT,
        );
        decline.push(jcc_patch(buf, 0x84)); // JZ: legacy layout
                                            // CMP QWORD [R9+value],0 (REX.W+B 83 /7 ib): not yet constructed.
        buf.emit(&[0x49, 0x83, 0xB9]);
        buf.emit(&plan.value_off.to_le_bytes());
        buf.emit_byte(0x00);
        decline.push(jcc_patch(buf, 0x85)); // JNE
    }
    buf.emit(&[0x8B, 0x4A, len_d8]); // MOV ECX,[RDX+len]: the char count
    buf.emit(&[0x85, 0xC9]); // TEST ECX,ECX
    decline.push(jcc_patch(buf, 0x84)); // JZ: the JDK's shared "".value

    // ---- the coder: R10 / R11 / R8 / RAX scratch; RCX, RDX, R9 live --------
    mov_imm64_full(buf, RAX, site.compact_flag_addr as i64); // Cast: the flag's address
    cmp_mem8_imm8(buf, RAX, 0, 0);
    let compact_off = jcc_patch(buf, 0x84); // JE: always UTF16
    buf.emit(&[0x4C, 0x8D, 0x52, plan.data_d8]); // LEA R10,[RDX+DATA]
    buf.emit(&[0x41, 0x89, 0xCB]); // MOV R11D,ECX: units left
                                   // Cast: the high byte of every 16-bit lane, as the imm64's bits.
    mov_imm64_full(buf, R8, 0xFF00_FF00_FF00_FF00u64 as i64);
    let quad_top = buf.pos();
    buf.emit(&[0x41, 0x83, 0xFB, 0x04]); // CMP R11D,4
    let to_units = jcc_patch(buf, 0x82); // JB
    buf.emit(&[0x49, 0x8B, 0x02]); // MOV RAX,[R10]
    buf.emit(&[0x4C, 0x85, 0xC0]); // TEST RAX,R8
    let wide_quad = jcc_patch(buf, 0x85); // JNZ: a char above 0xFF
    add_r64_imm8(buf, R10, 8);
    buf.emit(&[0x41, 0x83, 0xEB, 0x04]); // SUB R11D,4
    let back = jmp_patch(buf);
    patch_back(buf, back, quad_top);
    patch_here(buf, to_units);
    let unit_top = buf.pos();
    buf.emit(&[0x45, 0x85, 0xDB]); // TEST R11D,R11D
    let narrow = jcc_patch(buf, 0x84); // JZ: every char fits
    buf.emit(&[0x41, 0x80, 0x7A, 0x01, 0x00]); // CMP BYTE [R10+1],0
    let wide_unit = jcc_patch(buf, 0x85); // JNE
    add_r64_imm8(buf, R10, 2);
    buf.emit(&[0x41, 0xFF, 0xCB]); // DEC R11D
    let back = jmp_patch(buf);
    patch_back(buf, back, unit_top);
    patch_here(buf, compact_off);
    patch_here(buf, wide_quad);
    patch_here(buf, wide_unit);
    buf.emit(&[0x48, 0x01, 0xC9]); // ADD RCX,RCX: UTF16, two bytes a char
    patch_here(buf, narrow);
    // RCX = the byte[]'s length (zero-extended), under the inline cap.
    buf.emit(&[0x48, 0x81, 0xF9]); // CMP RCX,imm32
    buf.emit(&plan.max_count.to_le_bytes());
    decline.push(jcc_patch(buf, 0x87)); // JA

    // ---- thread, then the bump (both objects for valueOf) -------------------
    load_thread_r10(buf, site.thread);
    test_r64_r64(buf, R10);
    decline.push(jcc_patch(buf, 0x84)); // JZ no thread -> call
    mov_r64_mem(buf, R11, R10, site.cursor_off); // R11 = align8(cursor): the byte[]
    add_r64_imm8(buf, R11, 7);
    and_r64_imm8(buf, R11, -8);
    buf.emit(&[0x41, 0x89, 0xC8]); // MOV R8D,ECX
    add_r64_imm8(buf, R8, 7);
    and_r64_imm8(buf, R8, -8);
    if init {
        buf.emit(&[0x4B, 0x8D, 0x44, 0x03, plan.data_d8]); // LEA RAX,[R11+R8+DATA]
    } else {
        buf.emit(&[0x4F, 0x8D, 0x4C, 0x03, plan.data_d8]); // LEA R9,[R11+R8+DATA]: the String
        lea_r64_mem(buf, RAX, R9, plan.string_total);
    }
    cmp_r64_mem(buf, RAX, R10, site.end_off);
    decline.push(jcc_patch(buf, 0x87)); // JA: TLAB full

    // ---- per-collector pre-checks (RAX, R8 scratch) -------------------------
    if site.gc.g1_same_region {
        emit_g1_same_region_check(buf, site.g1_table, decline);
    }
    if init && site.young_bounds_addr != 0 {
        emit_young_receiver_check(buf, site.young_bounds_addr, decline);
    }
    if site.gc.zgc_start_bits {
        let objs: &[u8] = if init { &[R11] } else { &[R11, R9] };
        emit_zgc_start_bit_precheck(buf, site.cursor_off, site.end_off, objs, decline);
    }
    // ---- no plain decline past this point: a payload the pass below rejects
    // commits its allocation first (see `abandon`) ----------------------------

    // byte[] header: class id 0, length, mark word, "no hash yet".
    mov_dword_mem_imm32(buf, R11, 0, 0);
    buf.emit(&[0x41, 0x89, 0x4B, len_d8]); // MOV [R11+len],ECX
    mov_dword_mem_imm32(
        buf,
        R11,
        cratonvm_types::MARK_WORD_OFFSET as i32, // Cast: small header constant
        plan.byte_array_mark,
    );
    mov_dword_mem_imm32(
        buf,
        R11,
        cratonvm_types::IDENTITY_HASH_OFFSET as i32, // Cast: small header constant
        0,
    );
    if !init {
        // String header and zeroed body, exactly as `emit_ir_tail`.
        // Cast: a ClassId fits in 32 bits.
        mov_dword_mem_imm32(buf, R9, site.class_id_off, plan.string_class_id as i32);
        let flags = cratonvm_types::GC_FLAG_COMPACT | cratonvm_types::GC_FLAG_HEADER;
        mov_dword_mem_imm32(
            buf,
            R9,
            cratonvm_types::MARK_WORD_OFFSET as i32, // Cast: small header constant
            i32::from(flags) << 24,
        );
        let mut off =
            i32::try_from(cratonvm_types::COMPACT_HEADER_SIZE).unwrap_or(plan.string_total);
        while off < plan.string_total {
            mov_dword_mem_imm32(buf, R9, off, 0);
            off += 4;
        }
    }
    // ---- the payload, ONE read of each char (rev12, correctness) -----------
    // JDK 25's `StringUTF16.compress(char[], off, len)` compresses into a
    // fresh buffer and judges the RESULT of that single pass, falling back to
    // a UTF16 copy whose own content decides; so the String it builds is
    // canonical (LATIN1 iff every char of its own content is <= 0xFF) even
    // when another thread writes the array meanwhile. The coder scan above only
    // SIZED the array; the payload pass below re-judges what it actually read:
    // * LATIN1: compress with a per-unit check; a wide unit (written since the
    //   scan) abandons;
    // * UTF16 under COMPACT_STRINGS: copy, then scan the PRIVATE copy; no unit
    //   above 0xFF (the wide one was written back since the scan) abandons.
    // An abandoned attempt COMMITS what it allocated (complete headers,
    // unreachable garbage, as the box prefixes leave a refused box -- the TLAB
    // past the cursor must stay zeroed for the next inline bump), touches no
    // receiver field, and takes the call, which reads the array afresh.
    // Receiver / String fields (`coder`, `value`) are stored only after the
    // payload is accepted.
    let mut abandon: Vec<usize> = Vec::new();
    buf.emit(&[0x3B, 0x4A, len_d8]); // CMP ECX,[RDX+len]: byte length == chars?
    let latin1 = jcc_patch(buf, 0x84); // JE
    lea_r64_mem(buf, RDX, RDX, data);
    lea_r64_mem(buf, R8, R11, data);
    // Whole units only: the source is a SHARED `char[]` (w14a race).
    emit_copy_units(buf); // ECX bytes: the little-endian units
                          // Compact strings off: UTF16 whatever the content, no judgement owed.
    mov_imm64_full(buf, RAX, site.compact_flag_addr as i64); // Cast: the flag's address
    cmp_mem8_imm8(buf, RAX, 0, 0);
    let utf16_kept = jcc_patch(buf, 0x84); // JE
    abandon.push(emit_private_utf16_has_no_wide_unit(buf, len_d8, data));
    patch_here(buf, utf16_kept);
    emit_coder_store_r9(buf, site.coder, 1);
    mov_mem_r64(buf, R9, R11, plan.value_off); // String.value = the byte[]
    let filled = jmp_patch(buf);
    patch_here(buf, latin1);
    lea_r64_mem(buf, RDX, RDX, data);
    lea_r64_mem(buf, R8, R11, data);
    abandon.extend(emit_chars_compress_checked(buf)); // ECX chars, each one byte
    if init {
        // Written, not assumed: the receiver's coder cell is the JDK's to set.
        emit_coder_store_r9(buf, site.coder, 0);
    }
    mov_mem_r64(buf, R9, R11, plan.value_off); // String.value = the byte[]
    patch_here(buf, filled);

    // ---- commit LAST ----------------------------------------------------------
    emit_from_chars_commit(buf, site, init);
    if !init {
        mov_r64_r64(buf, RAX, R9);
    }
    // ---- an abandoned payload: commit the garbage, then the call --------------
    let done = jmp_patch(buf);
    for p in abandon {
        patch_here(buf, p);
    }
    emit_from_chars_commit(buf, site, init);
    decline.push(jmp_patch(buf));
    patch_here(buf, done);
}

/// ECX bytes (an even count: whole UTF-16 units) from the `char[]` payload
/// at [RDX] to [R8], reading every unit WHOLE: aligned qwords (four units,
/// each inside one atomic 8-byte load), then one 16-bit load per remaining
/// unit. [`emit_copy`]'s byte tail is NOT usable on a `char[]` another thread
/// may write: it reads a unit as two separate bytes, which can combine the
/// high byte of one write with the low byte of another (`R20FuzzFStrChars2`
/// `race.bad`, host w14a: a 37-char array's last unit came out `0x100 | A`).
/// Clobbers RAX, RCX, RDX, R8 and the flags.
fn emit_copy_units(buf: &mut ExecutableBuffer) {
    let qword_top = buf.pos();
    buf.emit(&[0x83, 0xF9, 0x08]); // CMP ECX,8
    let to_units = jcc_patch(buf, 0x82); // JB
    buf.emit(&[0x48, 0x8B, 0x02]); // MOV RAX,[RDX]
    buf.emit(&[0x49, 0x89, 0x00]); // MOV [R8],RAX
    buf.emit(&[0x48, 0x83, 0xC2, 0x08]); // ADD RDX,8
    buf.emit(&[0x49, 0x83, 0xC0, 0x08]); // ADD R8,8
    buf.emit(&[0x83, 0xE9, 0x08]); // SUB ECX,8
    let back = jmp_patch(buf);
    patch_back(buf, back, qword_top);
    patch_here(buf, to_units);
    let unit_top = buf.pos();
    buf.emit(&[0x83, 0xF9, 0x02]); // CMP ECX,2
    let done = jcc_patch(buf, 0x82); // JB: no whole unit left
    buf.emit(&[0x0F, 0xB7, 0x02]); // MOVZX EAX,WORD [RDX]
    buf.emit(&[0x66, 0x41, 0x89, 0x00]); // MOV [R8],AX
    buf.emit(&[0x48, 0x83, 0xC2, 0x02]); // ADD RDX,2
    buf.emit(&[0x49, 0x83, 0xC0, 0x02]); // ADD R8,2
    buf.emit(&[0x83, 0xE9, 0x02]); // SUB ECX,2
    let back = jmp_patch(buf);
    patch_back(buf, back, unit_top);
    patch_here(buf, done);
}

/// The commit of [`emit_ir_string_from_chars`]: R11 = the `byte[]` (header
/// written), R9 = the String for `!init`. ZGC start bits before the cursor
/// commit under `CRATONVM_ZGC_JIT_START_BIT_BEFORE_COMMIT`, after it
/// otherwise, as in `emit_ir_to_char_array`. Clobbers RAX, RCX, RDX, R8, R10
/// and the flags; R9 and R11 survive.
fn emit_from_chars_commit(buf: &mut ExecutableBuffer, site: &CharsIrSite, init: bool) {
    let plan = &site.alloc;
    let len_d8 = plan.len_d8;
    load_thread_r10(buf, site.thread);
    let bits_first =
        site.gc.zgc_start_bits && super::objects::zgc_start_bit_before_commit_enabled();
    if bits_first {
        emit_zgc_start_bit_store_prechecked(buf, site.cursor_off, R11);
        if !init {
            emit_zgc_start_bit_store_prechecked(buf, site.cursor_off, R9);
        }
    }
    if init {
        buf.emit(&[0x41, 0x8B, 0x4B, len_d8]); // MOV ECX,[R11+len]
        buf.emit(&[0x48, 0x8D, 0x41, 0x07]); // LEA RAX,[RCX+7]
        and_r64_imm8(buf, RAX, -8);
        buf.emit(&[0x49, 0x8D, 0x44, 0x03, plan.data_d8]); // LEA RAX,[R11+RAX+DATA]
    } else {
        lea_r64_mem(buf, RAX, R9, plan.string_total);
    }
    mov_mem_r64(buf, R10, RAX, site.cursor_off);
    if site.gc.zgc_start_bits && !bits_first {
        emit_zgc_start_bit_store_prechecked(buf, site.cursor_off, R11);
        if !init {
            emit_zgc_start_bit_store_prechecked(buf, site.cursor_off, R9);
        }
    }
}

/// Scan the PRIVATE UTF16 `byte[]` in R11 (its header length is the byte
/// count) for a unit above `0xFF`; answers the patch of the "none found"
/// edge, the fall-through is "found". Clobbers RAX, RCX, RDX, R10 and the
/// flags.
fn emit_private_utf16_has_no_wide_unit(buf: &mut ExecutableBuffer, len_d8: u8, data: i32) -> usize {
    lea_r64_mem(buf, RDX, R11, data);
    buf.emit(&[0x41, 0x8B, 0x4B, len_d8]); // MOV ECX,[R11+len]
    buf.emit(&[0xD1, 0xE9]); // SHR ECX,1: units
                             // Cast: the high byte of every 16-bit lane, as the imm64's bits.
    mov_imm64_full(buf, R10, 0xFF00_FF00_FF00_FF00u64 as i64);
    let quad_top = buf.pos();
    buf.emit(&[0x83, 0xF9, 0x04]); // CMP ECX,4
    let to_units = jcc_patch(buf, 0x82); // JB
    buf.emit(&[0x48, 0x8B, 0x02]); // MOV RAX,[RDX]
    buf.emit(&[0x4C, 0x85, 0xD0]); // TEST RAX,R10
    let found_quad = jcc_patch(buf, 0x85); // JNZ
    add_r64_imm8(buf, RDX, 8);
    buf.emit(&[0x83, 0xE9, 0x04]); // SUB ECX,4
    let back = jmp_patch(buf);
    patch_back(buf, back, quad_top);
    patch_here(buf, to_units);
    let unit_top = buf.pos();
    buf.emit(&[0x85, 0xC9]); // TEST ECX,ECX
    let none = jcc_patch(buf, 0x84); // JZ: every unit fits
    buf.emit(&[0x80, 0x7A, 0x01, 0x00]); // CMP BYTE [RDX+1],0
    let found_unit = jcc_patch(buf, 0x85); // JNE
    add_r64_imm8(buf, RDX, 2);
    buf.emit(&[0xFF, 0xC9]); // DEC ECX
    let back = jmp_patch(buf);
    patch_back(buf, back, unit_top);
    patch_here(buf, found_quad);
    patch_here(buf, found_unit);
    none
}

/// ECX UTF-16LE units from [RDX] to ECX Latin-1 bytes at [R8], CHECKING each
/// unit as it is read (one read per unit): a unit above `0xFF` takes one of
/// the returned edges with the copy unfinished. Eight per step under
/// `CRATONVM_JIT_IR_STRING_SSE2` (`PSRLW` + `PACKUSWB` of the high bytes,
/// tested through `MOVQ RAX`, then `PACKUSWB` of the units themselves), then
/// one at a time. Clobbers RAX, RCX, RDX, R8, XMM0, XMM1 and the flags.
fn emit_chars_compress_checked(buf: &mut ExecutableBuffer) -> Vec<usize> {
    let mut wide: Vec<usize> = Vec::new();
    if crate::ir_string_sse2_enabled() {
        let top = buf.pos();
        buf.emit(&[0x83, 0xF9, 0x08]); // CMP ECX,8
        let tail = jcc_patch(buf, 0x82); // JB
        buf.emit(&[0xF3, 0x0F, 0x6F, 0x02]); // MOVDQU XMM0,[RDX]
        buf.emit(&[0x66, 0x0F, 0x6F, 0xC8]); // MOVDQA XMM1,XMM0
        buf.emit(&[0x66, 0x0F, 0x71, 0xD1, 0x08]); // PSRLW XMM1,8: the high bytes
        buf.emit(&[0x66, 0x0F, 0x67, 0xC9]); // PACKUSWB XMM1,XMM1
        buf.emit(&[0x66, 0x48, 0x0F, 0x7E, 0xC8]); // MOVQ RAX,XMM1
        buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
        wide.push(jcc_patch(buf, 0x85)); // JNZ: a unit above 0xFF
        buf.emit(&[0x66, 0x0F, 0x67, 0xC0]); // PACKUSWB XMM0,XMM0 (exact now)
        buf.emit(&[0x66, 0x41, 0x0F, 0xD6, 0x00]); // MOVQ [R8],XMM0
        buf.emit(&[0x48, 0x83, 0xC2, 0x10]); // ADD RDX,16
        buf.emit(&[0x49, 0x83, 0xC0, 0x08]); // ADD R8,8
        buf.emit(&[0x83, 0xE9, 0x08]); // SUB ECX,8
        let back = jmp_patch(buf);
        patch_back(buf, back, top);
        patch_here(buf, tail);
    }
    let top = buf.pos();
    buf.emit(&[0x85, 0xC9]); // TEST ECX,ECX
    let done = jcc_patch(buf, 0x84); // JZ
    buf.emit(&[0x0F, 0xB7, 0x02]); // MOVZX EAX,WORD [RDX]
    buf.emit(&[0x3D, 0xFF, 0x00, 0x00, 0x00]); // CMP EAX,0xFF
    wide.push(jcc_patch(buf, 0x87)); // JA
    buf.emit(&[0x41, 0x88, 0x00]); // MOV [R8],AL
    buf.emit(&[0x48, 0x83, 0xC2, 0x02]); // ADD RDX,2
    buf.emit(&[0x49, 0xFF, 0xC0]); // INC R8
    buf.emit(&[0xFF, 0xC9]); // DEC ECX
    let back = jmp_patch(buf);
    patch_back(buf, back, top);
    patch_here(buf, done);
    wide
}

/// `coder = value` into the String in R9 at `coder` (`(offset, is_byte)`):
/// [`emit_coder_utf16_store`] for any value.
fn emit_coder_store_r9(buf: &mut ExecutableBuffer, coder: (i32, bool), value: u8) {
    let (off, is_byte) = coder;
    if is_byte {
        // MOV BYTE [R9+disp32],imm8: REX.B C6 /0, modrm 10 000 001.
        buf.emit(&[0x41, 0xC6, 0x81]);
        buf.emit(&off.to_le_bytes());
        buf.emit_byte(value);
    } else {
        mov_dword_mem_imm32(buf, R9, off, i32::from(value));
    }
}

/// The Generational `<init>` screen: R9 inside one of the two young
/// semispaces `JIT_REGION_BOUNDS` publishes (`[w0, w1)` or `[w2, w3)`, the
/// table `gen_heap::addr_in_published_young_regions` reads), else a decline
/// -- a young receiver's reference store owes no card. An empty span
/// (`0, 0`) matches nothing. Clobbers RAX and the flags.
fn emit_young_receiver_check(buf: &mut ExecutableBuffer, table: usize, decline: &mut Vec<usize>) {
    emit_young_object_check(buf, table, R9, decline);
}

/// [`emit_young_receiver_check`] for the object in `obj` (never RAX).
/// Clobbers RAX and the flags.
fn emit_young_object_check(
    buf: &mut ExecutableBuffer,
    table: usize,
    obj: u8,
    decline: &mut Vec<usize>,
) {
    debug_assert!(obj != RAX);
    mov_imm64_full(buf, RAX, table as i64); // Cast: the 'static table's address
    cmp_r64_mem(buf, obj, RAX, 0);
    let below_first = jcc_patch(buf, 0x82); // JB
    cmp_r64_mem(buf, obj, RAX, 8);
    let young = jcc_patch(buf, 0x82); // JB: inside the first
    patch_here(buf, below_first);
    cmp_r64_mem(buf, obj, RAX, 16);
    decline.push(jcc_patch(buf, 0x82)); // JB
    cmp_r64_mem(buf, obj, RAX, 24);
    decline.push(jcc_patch(buf, 0x83)); // JAE
    patch_here(buf, young);
}

/// Round rx27 wave 4 (lane splitfast; review item 2 of
/// `docs/feature-designs/jit-ir-optimizations.md`): the add prefix's screen of
/// `ArrayList.elementData` in RDX -- non-null, a one-dimensional reference
/// array (the whole kind-tags byte is `Array | Reference << 2`), and EXACTLY
/// `Object[]`: the header's class id (the `u32` at offset 0, which an array
/// header holds for its COMPONENT class) equals `object_class_id`. Any other
/// array is pushed onto `decline`: a reflectively swapped `String[]` must
/// reach the call, whose `aastore` throws `ArrayStoreException` for a
/// non-String element. Reads only; clobbers the flags.
pub(crate) fn emit_ir_object_array_screen(
    buf: &mut ExecutableBuffer,
    object_class_id: u32,
    decline: &mut Vec<usize>,
) {
    let ref_array_tags = (cratonvm_types::ObjectKind::Array as u8)
        | ((cratonvm_types::ArrayElementType::Reference as u8) << 2);
    let kind_d8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: x86-64 disp8
    test_r64_r64(buf, RDX);
    decline.push(jcc_patch(buf, 0x84)); // JZ: null
    buf.emit(&[0x80, 0x7A, kind_d8, ref_array_tags]); // CMP BYTE [RDX+KIND_TAGS], Array|Reference
    decline.push(jcc_patch(buf, 0x85)); // JNE: not a reference array
    buf.emit(&[0x81, 0x3A]); // CMP DWORD [RDX], imm32: the component class id
    buf.emit(&object_class_id.to_le_bytes());
    decline.push(jcc_patch(buf, 0x85)); // JNE: not exactly Object[]
}

/// Round rx27 wave 3, lane splitfast (`CRATONVM_JIT_IR_ARRAYLIST_ADD_PREFIX`):
/// the proof that storing the non-null reference R9 into the existing object
/// RDX (an `ArrayList.elementData`) owes the collector NOTHING, by the rules
/// the `new String(char[])` arm of [`emit_ir_string_from_chars`] uses for its
/// one store into a non-fresh receiver; anything else is pushed onto
/// `decline` (the call, whose `aastore` pays the barrier):
/// * G1: RDX and R9 in one region (an intra-region edge needs no remembered
///   set entry);
/// * Generational: RDX inside a published young semispace (a young holder
///   owes no card);
/// * ZGC: the thread's armed table with no mark and no generational mode
///   (`blocked == 0`).
///
/// The SATB half is the caller's: it proves the overwritten slot null.
/// Nothing is stored. Clobbers RAX, R8, R10, R11 and the flags; RCX, RDX and
/// R9 survive.
pub(crate) fn emit_ir_ref_store_proof(
    buf: &mut ExecutableBuffer,
    site: &CharsIrSite,
    decline: &mut Vec<usize>,
) {
    if site.gc.g1_same_region {
        mov_r64_r64(buf, R11, RDX);
        emit_g1_same_region_check(buf, site.g1_table, decline);
    }
    if site.young_bounds_addr != 0 {
        emit_young_object_check(buf, site.young_bounds_addr, RDX, decline);
    }
    if site.gc.zgc_start_bits {
        load_thread_r10(buf, site.thread);
        test_r64_r64(buf, R10);
        decline.push(jcc_patch(buf, 0x84)); // JZ no thread -> call
        emit_zgc_start_bit_precheck(buf, site.cursor_off, site.end_off, &[], decline);
    }
}

/// Round rx27 wave 3, lane splitfast (`CRATONVM_JIT_IR_ARRAYS_COPY_OF_RANGE_PREFIX`):
/// JDK 25 `Arrays.copyOfRange(byte[] original, int from, int to)`:
///
/// ```text
/// if (from == 0 && to == original.length) return original.clone();
/// int newLength = to - from;
/// if (newLength < 0) throw new IllegalArgumentException(from + " > " + to);
/// byte[] copy = new byte[newLength];
/// System.arraycopy(original, from, copy, 0, Math.min(original.length - from, newLength));
/// return copy;
/// ```
///
/// On entry RDX = `original`, R10D = `from`, R9D = `to`. Answered inline: a
/// non-null `byte[]` with `0 <= from <= to <= length` gets a FRESH
/// `byte[to - from]` holding `original[from..to]` (the whole-array `clone()`
/// is the same answer: a fresh equal array). Declines (to the call, nothing
/// stored): a null or non-`byte[]` `original`, `from > to` (the IAE),
/// `from < 0` (the `arraycopy` AIOOBE), `to > length` (the zero-padded copy:
/// rare, and the padding would need a zeroed tail), past the inline cap, no
/// thread, a full TLAB, a ZGC pre-check miss. A primitive array: no reference
/// store, no barrier. Leaves RAX = the array. Clobbers RAX, RCX, RDX, R8,
/// R9, R10, R11 and the flags.
pub(crate) fn emit_ir_byte_copy_of_range(
    buf: &mut ExecutableBuffer,
    site: &CharsIrSite,
    decline: &mut Vec<usize>,
) {
    let plan = &site.alloc;
    let data = i32::from(plan.data_d8);
    let len_d8 = plan.len_d8;
    test_r64_r64(buf, RDX);
    decline.push(jcc_patch(buf, 0x84)); // JZ: null (the call's NPE)
    cmp_mem8_imm8(buf, RDX, i32::from(site.kind_d8), site.byte_array_tag);
    decline.push(jcc_patch(buf, 0x85)); // JNE: not a byte[]
    buf.emit(&[0x44, 0x8B, 0x5A, len_d8]); // MOV R11D,[RDX+len]
    buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D,R10D
    decline.push(jcc_patch(buf, 0x88)); // JS: from < 0
    buf.emit(&[0x45, 0x39, 0xCA]); // CMP R10D,R9D
    decline.push(jcc_patch(buf, 0x8F)); // JG: from > to (signed)
    buf.emit(&[0x45, 0x39, 0xD9]); // CMP R9D,R11D
    decline.push(jcc_patch(buf, 0x87)); // JA: to > length

    // ECX = to - from, RDX = original + from (the copy reads [RDX + DATA]).
    buf.emit(&[0x44, 0x89, 0xC9]); // MOV ECX,R9D
    buf.emit(&[0x44, 0x29, 0xD1]); // SUB ECX,R10D
    buf.emit(&[0x45, 0x89, 0xD2]); // MOV R10D,R10D (from >= 0: zero-extend)
    buf.emit(&[0x4C, 0x01, 0xD2]); // ADD RDX,R10
    buf.emit(&[0x48, 0x81, 0xF9]); // CMP RCX,imm32
    buf.emit(&plan.max_count.to_le_bytes());
    decline.push(jcc_patch(buf, 0x87)); // JA: past the inline cap

    // ---- thread, then the bump ----------------------------------------------
    load_thread_r10(buf, site.thread);
    test_r64_r64(buf, R10);
    decline.push(jcc_patch(buf, 0x84)); // JZ no thread -> call
    mov_r64_mem(buf, R11, R10, site.cursor_off); // R11 = align8(cursor): the byte[]
    add_r64_imm8(buf, R11, 7);
    and_r64_imm8(buf, R11, -8);
    buf.emit(&[0x41, 0x89, 0xC8]); // MOV R8D,ECX
    add_r64_imm8(buf, R8, 7);
    and_r64_imm8(buf, R8, -8);
    buf.emit(&[0x4B, 0x8D, 0x44, 0x03, plan.data_d8]); // LEA RAX,[R11+R8+DATA]
    cmp_r64_mem(buf, RAX, R10, site.end_off);
    decline.push(jcc_patch(buf, 0x87)); // JA: TLAB full
    if site.gc.zgc_start_bits {
        // RAX / R8 die here; RCX, RDX, R9, R10, R11 survive.
        emit_zgc_start_bit_precheck(buf, site.cursor_off, site.end_off, &[R11], decline);
    }
    // ---- no decline past this point -----------------------------------------

    // byte[] header: class id 0, length, mark word, "no hash yet".
    mov_dword_mem_imm32(buf, R11, 0, 0);
    buf.emit(&[0x41, 0x89, 0x4B, len_d8]); // MOV [R11+len],ECX
    mov_dword_mem_imm32(
        buf,
        R11,
        cratonvm_types::MARK_WORD_OFFSET as i32, // Cast: small header constant
        plan.byte_array_mark,
    );
    mov_dword_mem_imm32(
        buf,
        R11,
        cratonvm_types::IDENTITY_HASH_OFFSET as i32, // Cast: small header constant
        0,
    );
    lea_r64_mem(buf, RDX, RDX, data);
    lea_r64_mem(buf, R8, R11, data);
    emit_copy(buf); // clobbers RAX, RCX, RDX, R8

    // ---- commit LAST, then answer the array ---------------------------------
    // As `emit_ir_to_char_array`: the start bit may go in before the commit;
    // its store clobbers RAX, RCX, RDX and R8, so the new cursor is derived
    // after it, from the header.
    load_thread_r10(buf, site.thread);
    let bits_first =
        site.gc.zgc_start_bits && super::objects::zgc_start_bit_before_commit_enabled();
    if bits_first {
        emit_zgc_start_bit_store_prechecked(buf, site.cursor_off, R11);
    }
    buf.emit(&[0x41, 0x8B, 0x43, len_d8]); // MOV EAX,[R11+len]
    add_r64_imm8(buf, RAX, 7);
    and_r64_imm8(buf, RAX, -8);
    buf.emit(&[0x49, 0x8D, 0x44, 0x03, plan.data_d8]); // LEA RAX,[R11+RAX+DATA]
    mov_mem_r64(buf, R10, RAX, site.cursor_off);
    if site.gc.zgc_start_bits && !bits_first {
        emit_zgc_start_bit_store_prechecked(buf, site.cursor_off, R11);
    }
    mov_r64_r64(buf, RAX, R11);
}

/// ECX Latin-1 bytes from [RDX] to ECX UTF-16LE units at [R8], eight per
/// `PUNPCKLBW` step under `CRATONVM_JIT_IR_STRING_SSE2` (the prefix's SSE2
/// switch), then [`emit_inflate`]. Clobbers RAX, RCX, RDX, R8, XMM0, XMM1
/// and the flags.
fn emit_chars_inflate(buf: &mut ExecutableBuffer) {
    if crate::ir_string_sse2_enabled() {
        buf.emit(&[0x66, 0x0F, 0xEF, 0xC9]); // PXOR XMM1,XMM1
        let top = buf.pos();
        buf.emit(&[0x83, 0xF9, 0x08]); // CMP ECX,8
        let tail = jcc_patch(buf, 0x82); // JB
        buf.emit(&[0xF3, 0x0F, 0x7E, 0x02]); // MOVQ XMM0,[RDX]
        buf.emit(&[0x66, 0x0F, 0x60, 0xC1]); // PUNPCKLBW XMM0,XMM1
        buf.emit(&[0xF3, 0x41, 0x0F, 0x7F, 0x00]); // MOVDQU [R8],XMM0
        buf.emit(&[0x48, 0x83, 0xC2, 0x08]); // ADD RDX,8
        buf.emit(&[0x49, 0x83, 0xC0, 0x10]); // ADD R8,16
        buf.emit(&[0x83, 0xE9, 0x08]); // SUB ECX,8
        let back = jmp_patch(buf);
        patch_back(buf, back, top);
        patch_here(buf, tail);
    }
    emit_inflate(buf);
}

// ---- `StringBuilder.append(int)` / `toString()` (JIT r20 wave 18, lane xSb) -----
//
// STR20-2 (book `jit-r20-strings-proposals.md`): the two builder members the
// optimizing tier had no prefix kind for, so a method with either site stayed
// pinned to the single-pass backend. Both serve a LATIN1 builder only and
// decline everything else to the call (the JDK body: growth, inflation,
// `maybeLatin1` compression and every exception stay its own).

/// `AbstractStringBuilder.append(int)` for a LATIN1 builder that already has
/// room (JDK 25: `spaceNeeded = count + DecimalDigits.stringSize(i)`,
/// `ensureCapacitySameCoder` a no-op, `DecimalDigits.uncheckedGetCharsLatin1`
/// writing the digits backwards from `spaceNeeded`). On entry RCX = the
/// builder's `value` (a non-null LATIN1 `byte[]`, proved by the caller),
/// R11D = `count` (zero-extended), R10D = `i`. A `count + size` past
/// `value.length` (a growth, or a corrupt count) is pushed onto `decline`
/// before anything is stored; past it the digits are written at
/// `value[count..]` -- `Integer.toString`'s text, `-2147483648` for
/// `Integer.MIN_VALUE` -- and R9D = the new count, for the caller's `count`
/// store. No allocation, no reference store, no safepoint. Clobbers RAX, RCX,
/// RDX, R8, R9, R10 and the flags; R11 survives.
pub(crate) fn emit_sb_append_int_latin1(buf: &mut ExecutableBuffer, decline: &mut Vec<usize>) {
    // `[value + 8]` is the array length and `[value + 16 + i]` byte `i`.
    const _: () = assert!(
        cratonvm_types::ARRAY_LENGTH_OFFSET == 8 && cratonvm_types::ARRAY_DATA_OFFSET == 16,
        "the append(int) prefix bakes ARRAY_LENGTH_OFFSET 8 / ARRAY_DATA_OFFSET 16"
    );
    // R8 = stringSize(i): a sign, then the magnitude's digits.
    buf.emit(&[0x49, 0x63, 0xC2]); // MOVSXD RAX,R10D
    buf.emit(&[0x45, 0x31, 0xC0]); // XOR R8D,R8D
    buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
    let non_negative = jcc_patch(buf, 0x89); // JNS
    buf.emit(&[0x49, 0xFF, 0xC0]); // INC R8
    buf.emit(&[0x48, 0xF7, 0xD8]); // NEG RAX (MIN_VALUE: 2^31 in 64 bits)
    patch_here(buf, non_negative);
    // Clobbers RDX.
    emit_digit_count(buf, R8);
    // R9D = count + size; it must fit the array (unsigned: count < 2^31,
    // size <= 11).
    buf.emit(&[0x45, 0x89, 0xD9]); // MOV R9D,R11D
    buf.emit(&[0x45, 0x01, 0xC1]); // ADD R9D,R8D
    buf.emit(&[0x44, 0x3B, 0x49, 0x08]); // CMP R9D,[RCX+8]
    let growth = jcc_patch(buf, 0x87); // JA: needs growth
    decline.push(growth);
    // ---- no decline past this point ----
    // Capacity already supplied the end; RAX still holds the magnitude.
    // Keep the sign before the backward digit stores, without recounting size.
    buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D,R10D
    let positive = jcc_patch(buf, 0x89); // JNS
    buf.emit(&[0x4A, 0x8D, 0x54, 0x19, 0x10]); // LEA RDX,[RCX+R11+16]
    mov_byte_mem_imm8(buf, RDX, 0, b'-');
    patch_here(buf, positive);
    buf.emit(&[0x4E, 0x8D, 0x44, 0x09, 0x10]); // LEA R8,[RCX+R9+16]
    mov_r64_r64(buf, R10, R8);
    emit_unsigned_decimal_backwards(buf);
}

/// `StringBuilder.toString()` for a LATIN1 builder (JDK 25: `new
/// String(this)` -> `String(AbstractStringBuilder, Void)`'s LATIN1 arm,
/// `coder = LATIN1`, `value = Arrays.copyOfRange(val, 0, count)`): a fresh
/// `byte[count]` and String in one TLAB bump, as
/// [`emit_ir_string_from_chars`]' `valueOf` arm allocates them. On entry
/// RDX = the builder's `value` and ECX = its `count` (zero-extended); the
/// caller proved the receiver an exact `java/lang/StringBuilder` and its coder
/// LATIN1.
///
/// Declines (to the call, nothing stored): the String layout replaced,
/// compact strings off (no LATIN1 String may exist then), a null or
/// non-`byte[]` value, an EMPTY builder (left to the JDK body, whichever
/// String it answers), `count > value.length`, a result over the inline cap,
/// no thread, a full TLAB, a collector pre-check miss. The one reference
/// store, `String.value = the byte[]`, is into the fresh String and owes what
/// the `valueOf` arm's does: G1 the same-region test, ZGC the start-bit
/// pre-check, Generational nothing (a fresh, young holder). Leaves RAX = the
/// String. Clobbers RAX, RCX, RDX, R8, R9, R10, R11 and the flags.
pub(crate) fn emit_ir_sb_to_string(
    buf: &mut ExecutableBuffer,
    site: &CharsIrSite,
    decline: &mut Vec<usize>,
) {
    emit_ir_latin1_string_copy(buf, site, decline, false);
}

/// JIT r21 wave 3, lane strings (ZCP-3, the allocating half): a fresh LATIN1
/// String holding `CharacterDataLatin1.toLowerCase` of each byte of a LATIN1
/// `byte[]` -- `A-Z` and `0xC0..=0xDE` except `0xD7` gain `0x20`, every other
/// byte is itself (JDK 25 `CharacterDataLatin1.toLowerCase(int)`: `ch | 0x20`
/// when that lands in `a-z` or `0xE0..=0xFE` minus `0xF7`) -- in one TLAB bump,
/// with exactly [`emit_ir_sb_to_string`]'s entry contract (RDX = the array,
/// ECX = the byte count, zero-extended), declines and clobbers. The caller has
/// proved the result is the JDK's (`StringLatin1.toLowerCase`'s non-locale
/// arm: no `tr` / `az` / `lt`-sensitive byte, a non-null locale, at least one
/// byte that changes).
pub(crate) fn emit_ir_latin1_to_lower_string(
    buf: &mut ExecutableBuffer,
    site: &CharsIrSite,
    decline: &mut Vec<usize>,
) {
    emit_ir_latin1_string_copy(buf, site, decline, true);
}

/// The byte loop of [`emit_ir_latin1_to_lower_string`]: RDX = source bytes,
/// R8 = destination bytes, ECX = count; clobbers RAX, RCX, RDX, R8 (as
/// [`emit_copy`]).
fn emit_copy_latin1_lower(buf: &mut ExecutableBuffer) {
    let top = buf.pos();
    buf.emit(&[0x85, 0xC9]); // TEST ECX,ECX
    let done = jcc_patch(buf, 0x84); // JZ
    buf.emit(&[0x0F, 0xB6, 0x02]); // MOVZX EAX,BYTE [RDX]
    let mut store: Vec<usize> = Vec::new();
    buf.emit(&[0x3C, 0x41]); // CMP AL,0x41
    store.push(jcc_patch(buf, 0x82)); // JB: below `A`
    buf.emit(&[0x3C, 0x5A]); // CMP AL,0x5A
    let add = jcc_patch(buf, 0x86); // JBE: `A-Z`
    buf.emit(&[0x3C, 0xC0]); // CMP AL,0xC0
    store.push(jcc_patch(buf, 0x82)); // JB
    buf.emit(&[0x3C, 0xDE]); // CMP AL,0xDE
    store.push(jcc_patch(buf, 0x87)); // JA: `0xDF..=0xFF`
    buf.emit(&[0x3C, 0xD7]); // CMP AL,0xD7
    store.push(jcc_patch(buf, 0x84)); // JE: the multiplication sign
    patch_here(buf, add);
    buf.emit(&[0x04, 0x20]); // ADD AL,0x20
    for p in store {
        patch_here(buf, p);
    }
    buf.emit(&[0x41, 0x88, 0x00]); // MOV [R8],AL
    buf.emit(&[0x48, 0xFF, 0xC2]); // INC RDX
    buf.emit(&[0x49, 0xFF, 0xC0]); // INC R8
    buf.emit(&[0xFF, 0xC9]); // DEC ECX
    let back = jmp_patch(buf);
    patch_back(buf, back, top);
    patch_here(buf, done);
}

/// [`emit_ir_sb_to_string`]'s body, its payload copied as is or (`to_lower`)
/// through [`emit_copy_latin1_lower`].
fn emit_ir_latin1_string_copy(
    buf: &mut ExecutableBuffer,
    site: &CharsIrSite,
    decline: &mut Vec<usize>,
    to_lower: bool,
) {
    emit_ir_string_alloc_coded(buf, site, decline, true, false, to_lower);
}

/// [`emit_ir_sb_to_string`], or with `copy == false` (JIT round 21, lane ci:
/// the upper-case prefix's `0xDF` -> "SS" result, longer than its source) a
/// fresh LATIN1 String of ECX bytes whose payload is NOT written: RDX is not
/// read and the source checks (null, `byte[]`, `count <= value.length`) are
/// skipped. The caller writes all ECX payload bytes before any safepoint (the
/// array holds no reference, and nothing reads it before the String is
/// returned). Leaves RAX = the String; the same clobbers.
pub(crate) fn emit_ir_latin1_string_alloc(
    buf: &mut ExecutableBuffer,
    site: &CharsIrSite,
    decline: &mut Vec<usize>,
    copy: bool,
) {
    emit_ir_string_alloc_coded(buf, site, decline, copy, false, false);
}

/// JIT round 21, lane latin1up (`CRATONVM_JIT_IR_LATIN1_UPPER_UTF16`): the
/// uncopied form of [`emit_ir_latin1_string_alloc`] for a UTF16 result -- a
/// fresh String with `coder = UTF16` (1) over a fresh `byte[ECX]` whose
/// payload is NOT written (ECX = the BYTE count, `2 *` the units, an even
/// number the caller computed in 64 bits so the inline cap sees any
/// overflow). The same declines (all before the first store), the same
/// collector arms, the same contract: the caller writes all ECX payload bytes
/// before any safepoint. Leaves RAX = the String; the same clobbers.
pub(crate) fn emit_ir_utf16_string_alloc(
    buf: &mut ExecutableBuffer,
    site: &CharsIrSite,
    decline: &mut Vec<usize>,
) {
    emit_ir_string_alloc_coded(buf, site, decline, false, true, false);
}

/// [`emit_ir_latin1_string_alloc`] / [`emit_ir_utf16_string_alloc`]: `utf16`
/// stores `coder = UTF16` over the zeroed body (the `coder` slot the
/// allocation plan validated, as the wide concat tail stores it). `to_lower`
/// (with `copy`, [`emit_ir_latin1_string_copy`]) copies the payload through
/// [`emit_copy_latin1_lower`].
fn emit_ir_string_alloc_coded(
    buf: &mut ExecutableBuffer,
    site: &CharsIrSite,
    decline: &mut Vec<usize>,
    copy: bool,
    utf16: bool,
    to_lower: bool,
) {
    emit_ir_string_alloc_src(buf, site, decline, copy, utf16, to_lower, false);
}

/// Round rx27, lane splitfast (`CRATONVM_JIT_IR_STRING_SUBSTRING_PREFIX`):
/// `String.substring(int, int)` for a LATIN1 receiver, JDK 25's body
/// (`checkBoundsBeginEnd(begin, end, length)`; the receiver itself for
/// `begin == 0 && end == length`; else `StringLatin1.newString(value, begin,
/// end - begin)`: `""` for an empty range, else `new String(Arrays.copyOfRange(
/// value, begin, end), LATIN1)`), with the fresh `byte[]` and String in ONE
/// TLAB bump, as [`emit_ir_sb_to_string`] allocates them.
///
/// On entry RAX = the receiver (the caller proved it a non-null
/// `java/lang/String` whose coder is LATIN1), RCX = its `value`, R10D =
/// `beginIndex`, R9D = `endIndex`. Declines (to the call, nothing stored):
/// a null or non-`byte[]` value, `begin < 0`, `end > length` (a negative `end`
/// included, unsigned), `begin > end` -- every case the call throws its
/// `StringIndexOutOfBoundsException` for --, an EMPTY range (the call answers
/// the `""` literal, which this body cannot name), and every decline of the
/// allocation (the String layout replaced, compact strings off, the inline
/// cap, no thread, a full TLAB, a collector pre-check miss). The whole range
/// answers RAX unchanged (the receiver). Otherwise RAX = the fresh String.
/// Clobbers RCX, RDX, R8, R9, R10, R11 and the flags (and RAX on the fresh
/// arm).
pub(crate) fn emit_ir_latin1_substring(
    buf: &mut ExecutableBuffer,
    site: &CharsIrSite,
    decline: &mut Vec<usize>,
) {
    let len_d8 = site.alloc.len_d8;
    test_r64_r64(buf, RCX);
    decline.push(jcc_patch(buf, 0x84)); // JZ: no value
    cmp_mem8_imm8(buf, RCX, i32::from(site.kind_d8), site.byte_array_tag);
    decline.push(jcc_patch(buf, 0x85)); // JNE: not a byte[]
    buf.emit(&[0x44, 0x8B, 0x59, len_d8]); // MOV R11D,[RCX+len]: length()
    buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D,R10D
    decline.push(jcc_patch(buf, 0x88)); // JS: begin < 0
    buf.emit(&[0x45, 0x39, 0xD9]); // CMP R9D,R11D
    decline.push(jcc_patch(buf, 0x87)); // JA: end > length, or end < 0
    buf.emit(&[0x45, 0x39, 0xCA]); // CMP R10D,R9D
    decline.push(jcc_patch(buf, 0x87)); // JA: begin > end

    // `begin == 0 && end == length`: `return this` (RAX).
    buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D,R10D
    let part = jcc_patch(buf, 0x85); // JNZ
    buf.emit(&[0x45, 0x39, 0xD9]); // CMP R9D,R11D
    let whole = jcc_patch(buf, 0x84); // JE
    patch_here(buf, part);
    // ECX = end - begin (the alloc declines 0), RDX = value + begin: the
    // copy below reads `[RDX + DATA]`, i.e. `value[begin]` onwards.
    buf.emit(&[0x45, 0x29, 0xD1]); // SUB R9D,R10D
    buf.emit(&[0x45, 0x89, 0xD2]); // MOV R10D,R10D (begin >= 0: zero-extend)
    buf.emit(&[0x4A, 0x8D, 0x14, 0x11]); // LEA RDX,[RCX+R10]
    buf.emit(&[0x44, 0x89, 0xC9]); // MOV ECX,R9D

    // The source checks ran above (`begin + count <= value.length`), so the
    // allocation skips its own (which read RDX as an array header).
    emit_ir_string_alloc_src(buf, site, decline, true, false, false, true);
    patch_here(buf, whole);
}

/// [`emit_ir_string_alloc_coded`]; `src_prechecked` (with `copy`): RDX is
/// NOT an array base but `value + begin`, whose `count` bytes the caller has
/// proved inside a `byte[]` -- the null / `byte[]` / `count <= length` source
/// checks are skipped (they would read a header that is not there) and the
/// copy reads `[RDX + DATA]` on, exactly as for a base.
fn emit_ir_string_alloc_src(
    buf: &mut ExecutableBuffer,
    site: &CharsIrSite,
    decline: &mut Vec<usize>,
    copy: bool,
    utf16: bool,
    to_lower: bool,
    src_prechecked: bool,
) {
    let plan = &site.alloc;
    let data = i32::from(plan.data_d8);
    let len_d8 = plan.len_d8;
    // ---- String layout-replace guard (RAX, R11 scratch) --------------------
    mov_imm64_full(buf, R11, plan.replace_guard.0 as i64); // Cast: baked address
    mov_r32_mem(buf, RAX, R11, 0);
    buf.emit_byte(0x3D); // CMP EAX, imm32
    buf.emit(&(plan.replace_guard.1 as i32).to_le_bytes()); // Cast: counter bits
    decline.push(jcc_patch(buf, 0x85)); // JNE

    // ---- compact strings, the value, the count -------------------------------
    mov_imm64_full(buf, RAX, site.compact_flag_addr as i64); // Cast: the flag's address
    cmp_mem8_imm8(buf, RAX, 0, 0);
    decline.push(jcc_patch(buf, 0x84)); // JE: compact strings off
    if copy && !src_prechecked {
        test_r64_r64(buf, RDX);
        decline.push(jcc_patch(buf, 0x84)); // JZ: no payload
        cmp_mem8_imm8(buf, RDX, i32::from(site.kind_d8), site.byte_array_tag);
        decline.push(jcc_patch(buf, 0x85)); // JNE: not a byte[]
    }
    buf.emit(&[0x85, 0xC9]); // TEST ECX,ECX
    decline.push(jcc_patch(buf, 0x84)); // JZ: an empty builder
    if copy && !src_prechecked {
        buf.emit(&[0x3B, 0x4A, len_d8]); // CMP ECX,[RDX+len]
        decline.push(jcc_patch(buf, 0x87)); // JA: count past the array
    }
    buf.emit(&[0x48, 0x81, 0xF9]); // CMP RCX,imm32
    buf.emit(&plan.max_count.to_le_bytes());
    decline.push(jcc_patch(buf, 0x87)); // JA: past the inline cap

    // ---- thread, then ONE bump for both objects -----------------------------
    load_thread_r10(buf, site.thread);
    test_r64_r64(buf, R10);
    decline.push(jcc_patch(buf, 0x84)); // JZ no thread -> call
    mov_r64_mem(buf, R11, R10, site.cursor_off); // R11 = align8(cursor): the byte[]
    add_r64_imm8(buf, R11, 7);
    and_r64_imm8(buf, R11, -8);
    buf.emit(&[0x41, 0x89, 0xC8]); // MOV R8D,ECX
    add_r64_imm8(buf, R8, 7);
    and_r64_imm8(buf, R8, -8);
    buf.emit(&[0x4F, 0x8D, 0x4C, 0x03, plan.data_d8]); // LEA R9,[R11+R8+DATA]: the String
    lea_r64_mem(buf, RAX, R9, plan.string_total);
    cmp_r64_mem(buf, RAX, R10, site.end_off);
    decline.push(jcc_patch(buf, 0x87)); // JA: TLAB full

    // ---- per-collector pre-checks (RAX, R8 scratch) -------------------------
    if site.gc.g1_same_region {
        emit_g1_same_region_check(buf, site.g1_table, decline);
    }
    if site.gc.zgc_start_bits {
        emit_zgc_start_bit_precheck(buf, site.cursor_off, site.end_off, &[R11, R9], decline);
    }
    // ---- no decline past this point: every store below is committed --------

    // byte[] header: class id 0, length, mark word, "no hash yet".
    mov_dword_mem_imm32(buf, R11, 0, 0);
    buf.emit(&[0x41, 0x89, 0x4B, len_d8]); // MOV [R11+len],ECX
    mov_dword_mem_imm32(
        buf,
        R11,
        cratonvm_types::MARK_WORD_OFFSET as i32, // Cast: small header constant
        plan.byte_array_mark,
    );
    mov_dword_mem_imm32(
        buf,
        R11,
        cratonvm_types::IDENTITY_HASH_OFFSET as i32, // Cast: small header constant
        0,
    );
    // String header and zeroed body (`coder` LATIN1 = 0, `hash` 0,
    // `hashIsZero` false), exactly as `emit_ir_tail`.
    // Cast: a ClassId fits in 32 bits.
    mov_dword_mem_imm32(buf, R9, site.class_id_off, plan.string_class_id as i32);
    let flags = cratonvm_types::GC_FLAG_COMPACT | cratonvm_types::GC_FLAG_HEADER;
    mov_dword_mem_imm32(
        buf,
        R9,
        cratonvm_types::MARK_WORD_OFFSET as i32, // Cast: small header constant
        i32::from(flags) << 24,
    );
    let mut off = i32::try_from(cratonvm_types::COMPACT_HEADER_SIZE).unwrap_or(plan.string_total);
    while off < plan.string_total {
        mov_dword_mem_imm32(buf, R9, off, 0);
        off += 4;
    }
    if utf16 {
        emit_coder_store_r9(buf, site.coder, 1);
    }
    // The payload: `count` bytes of the builder's array (ECX, RDX), then
    // `String.value = the byte[]`.
    if copy {
        lea_r64_mem(buf, RDX, RDX, data);
        lea_r64_mem(buf, R8, R11, data);
        if to_lower {
            emit_copy_latin1_lower(buf); // clobbers RAX, RCX, RDX, R8
        } else {
            emit_copy(buf); // clobbers RAX, RCX, RDX, R8
        }
    }
    mov_mem_r64(buf, R9, R11, plan.value_off);

    // ---- commit LAST, then answer the String --------------------------------
    emit_from_chars_commit(buf, site, false);
    mov_r64_r64(buf, RAX, R9);
}

/// `CRATONVM_JIT_IR_CONCAT_INLINE` (JIT round 19 proposal wave 5, lane
/// concat5, STRGAP4-1; default OFF): the optimizing tier lowers an admitted
/// bridged concat (`Op::IndyBridge`, itself behind
/// `CRATONVM_JIT_IR_INDY_BRIDGE`) inline, with the bridge call as its decline
/// edge. Read once per site of a compile, no process global.
pub(crate) fn ir_concat_inline_enabled() -> bool {
    true
        || crate::ir::ir_indy_bridge_with_inline_enabled()
}

/// JIT round 19 proposal wave 5, lane concat5: the shared passes executed over
/// a fake frame and fake Strings (every operand kind, every decline the
/// length pass owns), the optimizing tier's whole tail executed against a fake
/// thread and TLAB, and the collector pre-checks pinned byte for byte to the
/// single-pass `Compiler` forms they copy.
#[cfg(all(test, target_arch = "x86_64"))]
mod r19_concat5_shared_emit_tests;

/// JIT r20 wave 12, lane qStr (W6-L2-2): the `char[]` copy tails
/// ([`emit_ir_to_char_array`], [`emit_ir_string_from_chars`]) EXECUTED against
/// a fake thread and TLAB, on both arms of `CRATONVM_JIT_IR_STRING_SSE2`: the
/// objects they build, the coder they choose (the JDK's `COMPACT_STRINGS` +
/// `StringUTF16.compress` answer) and every decline, which must leave the
/// cursor where it was.
#[cfg(all(test, target_arch = "x86_64"))]
mod r20_qstr_chars_tests;

/// JIT r20 wave 18, lane xSb (STR20-2): the two builder emitters
/// ([`emit_sb_append_int_latin1`], [`emit_ir_sb_to_string`]) EXECUTED over
/// fake arrays, a fake thread and a fake TLAB: the digit text (every sign and
/// width, `Integer.MIN_VALUE` / `MAX_VALUE`) and the new count, the fresh
/// LATIN1 String, and every decline leaving the array / cursor untouched.
#[cfg(all(test, target_arch = "x86_64"))]
mod r20_xsb_builder_tests;

/// Round rx27 wave 4 (lane splitfast; review item 2):
/// [`emit_ir_object_array_screen`] EXECUTED over fake headers: only a
/// one-dimensional reference array whose component class id is
/// `java/lang/Object`'s passes; a `String[]` (another component), a primitive
/// array, a plain object and null decline, so the `ArrayList.add` prefix
/// leaves a swapped `elementData` to the call's `aastore`.
#[cfg(all(test, target_arch = "x86_64"))]
mod rx27_w4_object_array_screen_tests;

/// JIT round 21, lane latin1up (`CRATONVM_JIT_IR_LATIN1_UPPER_UTF16`):
/// [`emit_ir_utf16_string_alloc`] EXECUTED over a fake thread and TLAB -- a
/// fresh `byte[RCX]` and String with `coder = UTF16`, and every decline
/// (including a 64-bit byte count past 2^32) leaving the cursor untouched.
#[cfg(all(test, target_arch = "x86_64"))]
mod jr21_latin1up_tests;

#[cfg(all(test, target_arch = "x86_64"))]
mod known_end_decimal_tests;

/// Screen `reg` (`R9` or `R10`) as a non-null `byte[]` of length 1 and load its
/// unsigned byte into `reg`'s 32-bit half, declining otherwise.
pub(crate) fn emit_ir_latin1_len1_char(
    buf: &mut ExecutableBuffer,
    site: &CharsIrSite,
    reg: u8,
    decline: &mut Vec<usize>,
) {
    debug_assert!(matches!(reg, R9 | R10));
    let rm = reg & 7;
    test_r64_r64(buf, reg);
    decline.push(jcc_patch(buf, 0x84)); // JZ
    cmp_mem8_imm8(buf, reg, i32::from(site.kind_d8), site.byte_array_tag);
    decline.push(jcc_patch(buf, 0x85)); // JNE
    buf.emit(&[0x41, 0x83, 0x78 | rm, site.alloc.len_d8, 0x01]); // CMP DWORD [reg+len], 1
    decline.push(jcc_patch(buf, 0x85)); // JNE
    buf.emit(&[0x45, 0x0F, 0xB6, 0x40 | (rm << 3) | rm, site.alloc.data_d8]); // MOVZX reg32, [reg+data]
}

/// `String.replace(char, char)` (`!unchanged_zero`, answering `RAX` when no
/// replacement occurs) and `StringLatin1.replace(byte[], char, char)`
/// (`unchanged_zero`, answering `null` = 0 when `oldChar > 0xFF` or `oldChar`
/// is absent) for a LATIN1 `byte[]` in `RCX`, `oldChar` in `R10D`, `newChar`
/// in `R9D`, and (when `!unchanged_zero`) the receiver String in `RAX`.
///
/// Uses only `XMM0`/`XMM1` (`IR_FP_SCRATCH`) and caller-saved scratch GPRs
/// (`RAX, RCX, RDX, R8..R11`). Every decline (null/non-`byte[]` payload,
/// `newChar > 0xFF` when `oldChar` is present, or TLAB/layout/GC refusal)
/// precedes the first store.
pub(crate) fn emit_ir_latin1_replace_char(
    buf: &mut ExecutableBuffer,
    site: &CharsIrSite,
    decline: &mut Vec<usize>,
    unchanged_zero: bool,
) {
    fn emit_replace_16(buf: &mut ExecutableBuffer, data_d8: u8) {
        buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]); // MOVD XMM0, EAX (splat4(old))
        buf.emit(&[0x66, 0x0F, 0x70, 0xC0, 0x00]); // PSHUFD XMM0, XMM0, 0
        buf.emit(&[0xF3, 0x0F, 0x6F, 0x4C, 0x11, data_d8]); // MOVDQU XMM1, [RCX+RDX+data]
        buf.emit(&[0x66, 0x0F, 0x74, 0xC8]); // PCMPEQB XMM1, XMM0
        buf.emit(&[0x66, 0x41, 0x0F, 0x6E, 0xC2]); // MOVD XMM0, R10D (splat4(old ^ new))
        buf.emit(&[0x66, 0x0F, 0x70, 0xC0, 0x00]); // PSHUFD XMM0, XMM0, 0
        buf.emit(&[0x66, 0x0F, 0xDB, 0xC8]); // PAND XMM1, XMM0
        buf.emit(&[0xF3, 0x0F, 0x6F, 0x44, 0x11, data_d8]); // MOVDQU XMM0, [RCX+RDX+data]
        buf.emit(&[0x66, 0x0F, 0xEF, 0xC8]); // PXOR XMM1, XMM0
        buf.emit(&[0xF3, 0x41, 0x0F, 0x7F, 0x4C, 0x13, data_d8]); // MOVDQU [R11+RDX+data], XMM1
    }

    let len_d8 = site.alloc.len_d8;
    let data_d8 = site.alloc.data_d8;
    test_r64_r64(buf, RCX);
    decline.push(jcc_patch(buf, 0x84)); // JZ: null value
    cmp_mem8_imm8(buf, RCX, i32::from(site.kind_d8), site.byte_array_tag);
    decline.push(jcc_patch(buf, 0x85)); // JNE: not byte[]
    buf.emit(&[0x44, 0x8B, 0x59, len_d8]); // MOV R11D, [RCX+len]
    buf.emit(&[0x45, 0x85, 0xDB]); // TEST R11D, R11D
    decline.push(jcc_patch(buf, 0x88)); // JS

    buf.emit(&[0x45, 0x0F, 0xB7, 0xD2]); // MOVZX R10D, R10W
    buf.emit(&[0x45, 0x0F, 0xB7, 0xC9]); // MOVZX R9D, R9W

    let mut unchanged: Vec<usize> = Vec::new();
    if !unchanged_zero {
        buf.emit(&[0x45, 0x39, 0xCA]); // CMP R10D, R9D
        unchanged.push(jcc_patch(buf, 0x84)); // JE: oldChar == newChar
    }
    buf.emit(&[0x41, 0x81, 0xFA, 0xFF, 0x00, 0x00, 0x00]); // CMP R10D, 0xFF
    unchanged.push(jcc_patch(buf, 0x87)); // JA: !canEncode(oldChar)

    // Broadcast oldChar byte into XMM0 without clobbering RAX.
    buf.emit(&[0x41, 0x69, 0xD2, 0x01, 0x01, 0x01, 0x01]); // IMUL EDX, R10D, 0x01010101
    buf.emit(&[0x66, 0x0F, 0x6E, 0xC2]); // MOVD XMM0, EDX
    buf.emit(&[0x66, 0x0F, 0x70, 0xC0, 0x00]); // PSHUFD XMM0, XMM0, 0

    let mut found: Vec<usize> = Vec::new();
    buf.emit(&[0x31, 0xD2]); // XOR EDX, EDX
    buf.emit(&[0x41, 0x83, 0xFB, 0x10]); // CMP R11D, 16
    let scan_short = jcc_patch(buf, 0x82); // JB

    let scan_vec_top = buf.pos();
    buf.emit(&[0xF3, 0x0F, 0x6F, 0x4C, 0x11, data_d8]); // MOVDQU XMM1, [RCX+RDX+data]
    buf.emit(&[0x66, 0x0F, 0x74, 0xC8]); // PCMPEQB XMM1, XMM0
    buf.emit(&[0x66, 0x44, 0x0F, 0xD7, 0xC1]); // PMOVMSKB R8D, XMM1
    buf.emit(&[0x45, 0x85, 0xC0]); // TEST R8D, R8D
    found.push(jcc_patch(buf, 0x85)); // JNZ
    buf.emit(&[0x83, 0xC2, 0x10]); // ADD EDX, 16
    buf.emit(&[0x44, 0x8D, 0x42, 0x10]); // LEA R8D, [RDX+16]
    buf.emit(&[0x45, 0x39, 0xD8]); // CMP R8D, R11D
    let scan_back = jcc_patch(buf, 0x86); // JBE
    patch_back(buf, scan_back, scan_vec_top);

    buf.emit(&[0x44, 0x39, 0xDA]); // CMP EDX, R11D
    unchanged.push(jcc_patch(buf, 0x83)); // JAE
    buf.emit(&[0x41, 0x8D, 0x53, 0xF0]); // LEA EDX, [R11-16]
    buf.emit(&[0xF3, 0x0F, 0x6F, 0x4C, 0x11, data_d8]); // MOVDQU XMM1, [RCX+RDX+data]
    buf.emit(&[0x66, 0x0F, 0x74, 0xC8]); // PCMPEQB XMM1, XMM0
    buf.emit(&[0x66, 0x44, 0x0F, 0xD7, 0xC1]); // PMOVMSKB R8D, XMM1
    buf.emit(&[0x45, 0x85, 0xC0]); // TEST R8D, R8D
    found.push(jcc_patch(buf, 0x85)); // JNZ
    unchanged.push(jmp_patch(buf));

    patch_here(buf, scan_short);
    let short_top = buf.pos();
    buf.emit(&[0x44, 0x39, 0xDA]); // CMP EDX, R11D
    unchanged.push(jcc_patch(buf, 0x83)); // JAE
    buf.emit(&[0x44, 0x0F, 0xB6, 0x44, 0x11, data_d8]); // MOVZX R8D, BYTE [RCX+RDX+data]
    buf.emit(&[0x45, 0x39, 0xD0]); // CMP R8D, R10D
    found.push(jcc_patch(buf, 0x84)); // JE
    buf.emit(&[0xFF, 0xC2]); // INC EDX
    let short_back = jmp_patch(buf);
    patch_back(buf, short_back, short_top);

    for p in unchanged {
        patch_here(buf, p);
    }
    if unchanged_zero {
        buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
    }
    let done_unchanged = jmp_patch(buf);

    for p in found {
        patch_here(buf, p);
    }
    buf.emit(&[0x41, 0x81, 0xF9, 0xFF, 0x00, 0x00, 0x00]); // CMP R9D, 0xFF
    decline.push(jcc_patch(buf, 0x87)); // JA: UTF-16 inflation declines to call

    // Preserve src_arr in XMM0 and `old | ((old ^ new) << 8)` in XMM1 across alloc.
    buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC1]); // MOVQ XMM0, RCX
    buf.emit(&[0x45, 0x31, 0xD1]); // XOR R9D, R10D
    buf.emit(&[0x41, 0xC1, 0xE1, 0x08]); // SHL R9D, 8
    buf.emit(&[0x45, 0x09, 0xCA]); // OR R10D, R9D
    buf.emit(&[0x66, 0x41, 0x0F, 0x6E, 0xCA]); // MOVD XMM1, R10D
    buf.emit(&[0x44, 0x89, 0xD9]); // MOV ECX, R11D
    emit_ir_latin1_string_alloc(buf, site, decline, false);

    // R11 = fresh byte[len], R9 = RAX = fresh String.
    buf.emit(&[0x66, 0x48, 0x0F, 0x7E, 0xC1]); // MOVQ RCX, XMM0 (src_arr)
    buf.emit(&[0x45, 0x8B, 0x43, len_d8]); // MOV R8D, [R11+len]
    buf.emit(&[0x66, 0x41, 0x0F, 0x7E, 0xCA]); // MOVD R10D, XMM1
    buf.emit(&[0x41, 0x0F, 0xB6, 0xC2]); // MOVZX EAX, R10B (old)
    buf.emit(&[0x41, 0xC1, 0xEA, 0x08]); // SHR R10D, 8 (old ^ new)
    buf.emit(&[0x31, 0xD2]); // XOR EDX, EDX
    buf.emit(&[0x41, 0x83, 0xF8, 0x10]); // CMP R8D, 16
    let copy_short = jcc_patch(buf, 0x82); // JB

    buf.emit(&[0x69, 0xC0, 0x01, 0x01, 0x01, 0x01]); // IMUL EAX, EAX, 0x01010101
    buf.emit(&[0x45, 0x69, 0xD2, 0x01, 0x01, 0x01, 0x01]); // IMUL R10D, R10D, 0x01010101
    buf.emit(&[0x41, 0x83, 0xE8, 0x10]); // SUB R8D, 16 (len - 16)
    let vec_top = buf.pos();
    emit_replace_16(buf, data_d8);
    buf.emit(&[0x83, 0xC2, 0x10]); // ADD EDX, 16
    buf.emit(&[0x44, 0x39, 0xC2]); // CMP EDX, R8D
    let vec_back = jcc_patch(buf, 0x86); // JBE
    patch_back(buf, vec_back, vec_top);

    buf.emit(&[0x8D, 0x52, 0xF0]); // LEA EDX, [RDX-16]
    buf.emit(&[0x44, 0x39, 0xC2]); // CMP EDX, R8D
    let exact_16 = jcc_patch(buf, 0x84); // JE
    buf.emit(&[0x44, 0x89, 0xC2]); // MOV EDX, R8D (len - 16)
    emit_replace_16(buf, data_d8);
    patch_here(buf, exact_16);
    let vec_done = jmp_patch(buf);

    patch_here(buf, copy_short);
    buf.emit(&[0x66, 0x49, 0x0F, 0x6E, 0xC1]); // MOVQ XMM0, R9
    buf.emit(&[0x41, 0x89, 0xC1]); // MOV R9D, EAX (old)
    buf.emit(&[0x41, 0x31, 0xC2]); // XOR R10D, EAX (new)
    let cshort_top = buf.pos();
    buf.emit(&[0x44, 0x39, 0xC2]); // CMP EDX, R8D
    let cshort_done = jcc_patch(buf, 0x83); // JAE
    buf.emit(&[0x0F, 0xB6, 0x44, 0x11, data_d8]); // MOVZX EAX, BYTE [RCX+RDX+data]
    buf.emit(&[0x44, 0x39, 0xC8]); // CMP EAX, R9D
    buf.emit(&[0x41, 0x0F, 0x44, 0xC2]); // CMOVE EAX, R10D
    buf.emit(&[0x41, 0x88, 0x44, 0x13, data_d8]); // MOV BYTE [R11+RDX+data], AL
    buf.emit(&[0xFF, 0xC2]); // INC EDX
    let cshort_back = jmp_patch(buf);
    patch_back(buf, cshort_back, cshort_top);
    patch_here(buf, cshort_done);
    buf.emit(&[0x66, 0x49, 0x0F, 0x7E, 0xC1]); // MOVQ R9, XMM0

    patch_here(buf, vec_done);
    mov_r64_r64(buf, RAX, R9);
    patch_here(buf, done_unchanged);
}

/// `String.concat(String)` for two LATIN1 Strings: `RAX` = receiver, `RCX` =
/// `this.value`, `R10` = `arg.value`. Answers `RAX` unchanged when `arg` is
/// empty (`arg.length == 0`); otherwise allocates `byte[len1 + len2]` and the
/// result `String` in one TLAB bump and copies both payloads. Uses only
/// `XMM0`/`XMM1` and caller-saved scratch GPRs (`RAX, RCX, RDX, R8..R11`).
pub(crate) fn emit_ir_latin1_concat(
    buf: &mut ExecutableBuffer,
    site: &CharsIrSite,
    decline: &mut Vec<usize>,
) {
    let len_d8 = site.alloc.len_d8;
    let data = i32::from(site.alloc.data_d8);
    test_r64_r64(buf, RCX);
    decline.push(jcc_patch(buf, 0x84));
    cmp_mem8_imm8(buf, RCX, i32::from(site.kind_d8), site.byte_array_tag);
    decline.push(jcc_patch(buf, 0x85));
    test_r64_r64(buf, R10);
    decline.push(jcc_patch(buf, 0x84));
    cmp_mem8_imm8(buf, R10, i32::from(site.kind_d8), site.byte_array_tag);
    decline.push(jcc_patch(buf, 0x85));

    buf.emit(&[0x41, 0x8B, 0x52, len_d8]); // MOV EDX, [R10+len] (arg.length)
    buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
    decline.push(jcc_patch(buf, 0x88)); // JS
    let arg_empty = jcc_patch(buf, 0x84); // JZ -> return receiver RAX

    buf.emit(&[0x44, 0x8B, 0x41, len_d8]); // MOV R8D, [RCX+len] (this.length)
    buf.emit(&[0x45, 0x85, 0xC0]); // TEST R8D, R8D
    decline.push(jcc_patch(buf, 0x88)); // JS

    buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC1]); // MOVQ XMM0, RCX
    buf.emit(&[0x66, 0x49, 0x0F, 0x6E, 0xCA]); // MOVQ XMM1, R10
    buf.emit(&[0x44, 0x89, 0xC1]); // MOV ECX, R8D
    buf.emit(&[0x01, 0xD1]); // ADD ECX, EDX
    decline.push(jcc_patch(buf, 0x80)); // JO: i32 overflow

    emit_ir_latin1_string_alloc(buf, site, decline, false);

    buf.emit(&[0x66, 0x48, 0x0F, 0x7E, 0xC2]); // MOVQ RDX, XMM0 (this.value)
    buf.emit(&[0x8B, 0x4A, len_d8]); // MOV ECX, [RDX+len]
    lea_r64_mem(buf, RDX, RDX, data);
    lea_r64_mem(buf, R8, R11, data);
    emit_copy(buf);

    buf.emit(&[0x66, 0x48, 0x0F, 0x7E, 0xCA]); // MOVQ RDX, XMM1 (arg.value)
    buf.emit(&[0x8B, 0x4A, len_d8]); // MOV ECX, [RDX+len]
    lea_r64_mem(buf, RDX, RDX, data);
    emit_copy(buf);

    mov_r64_r64(buf, RAX, R9);
    patch_here(buf, arg_empty);
}

/// 4-way unrolled polynomial Latin-1 `String.hashCode()` over `R11D` bytes at
/// `[RCX + data_d8]`, leaving the 32-bit hash in `EAX`.
/// `h' = 31^4 * h + 31^3 * b0 + 31^2 * b1 + 31 * b2 + b3`, breaking the serial
/// `IMUL 31` dependency chain 4x. Clobbers `RAX, RDX, R8, R9, R10` and flags;
/// preserves `RCX`, `R11`, and all `XMM` registers.
pub(crate) fn emit_latin1_hash_unrolled4(buf: &mut ExecutableBuffer, data_d8: u8) {
    debug_assert!(data_d8 <= 124);
    buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX (h = 0)
    buf.emit(&[0x31, 0xD2]); // XOR EDX, EDX (i = 0)
    buf.emit(&[0x41, 0x83, 0xFB, 0x04]); // CMP R11D, 4
    let to_tail = jcc_patch(buf, 0x82); // JB

    buf.emit(&[0x45, 0x8D, 0x53, 0xFC]); // LEA R10D, [R11-4]
    let loop4_top = buf.pos();
    buf.emit(&[0x69, 0xC0]); // IMUL EAX, EAX, 923521 (31^4)
    buf.emit(&923_521i32.to_le_bytes());
    buf.emit(&[0x44, 0x0F, 0xB6, 0x44, 0x11, data_d8]); // MOVZX R8D, BYTE [RCX+RDX+data]
    buf.emit(&[0x45, 0x69, 0xC0]); // IMUL R8D, R8D, 29791 (31^3)
    buf.emit(&29_791i32.to_le_bytes());
    buf.emit(&[0x44, 0x0F, 0xB6, 0x4C, 0x11, data_d8 + 1]); // MOVZX R9D, BYTE [RCX+RDX+data+1]
    buf.emit(&[0x45, 0x69, 0xC9]); // IMUL R9D, R9D, 961 (31^2)
    buf.emit(&961i32.to_le_bytes());
    buf.emit(&[0x45, 0x01, 0xC8]); // ADD R8D, R9D
    buf.emit(&[0x44, 0x0F, 0xB6, 0x4C, 0x11, data_d8 + 2]); // MOVZX R9D, BYTE [RCX+RDX+data+2]
    buf.emit(&[0x45, 0x6B, 0xC9, 0x1F]); // IMUL R9D, R9D, 31
    buf.emit(&[0x45, 0x01, 0xC8]); // ADD R8D, R9D
    buf.emit(&[0x44, 0x0F, 0xB6, 0x4C, 0x11, data_d8 + 3]); // MOVZX R9D, BYTE [RCX+RDX+data+3]
    buf.emit(&[0x45, 0x01, 0xC8]); // ADD R8D, R9D
    buf.emit(&[0x44, 0x01, 0xC0]); // ADD EAX, R8D
    buf.emit(&[0x83, 0xC2, 0x04]); // ADD EDX, 4
    buf.emit(&[0x44, 0x39, 0xD2]); // CMP EDX, R10D
    let loop4_back = jcc_patch(buf, 0x86); // JBE
    patch_back(buf, loop4_back, loop4_top);

    patch_here(buf, to_tail);
    let tail_top = buf.pos();
    buf.emit(&[0x44, 0x39, 0xDA]); // CMP EDX, R11D
    let done = jcc_patch(buf, 0x83); // JAE
    buf.emit(&[0x44, 0x0F, 0xB6, 0x54, 0x11, data_d8]); // MOVZX R10D, BYTE [RCX+RDX+data]
    buf.emit(&[0x6B, 0xC0, 0x1F]); // IMUL EAX, EAX, 31
    buf.emit(&[0x44, 0x01, 0xD0]); // ADD EAX, R10D
    buf.emit(&[0xFF, 0xC2]); // INC EDX
    let tail_back = jmp_patch(buf);
    patch_back(buf, tail_back, tail_top);
    patch_here(buf, done);
}

#[cfg(all(test, target_arch = "x86_64"))]
mod concat21_6_boxed_wrapper_tests;

#[cfg(all(test, target_arch = "x86_64"))]
mod quarkus_replace_concat_tests;
