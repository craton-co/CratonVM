// SPDX-License-Identifier: Apache-2.0
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
            ConcatArgKind::Boolean => {
                // R8 += 4 + (low 32 bits == 0): "true" / "false".
                buf.emit(&[0x85, 0xC0]); // TEST EAX,EAX
                buf.emit(&[0x0F, 0x94, 0xC1]); // SETZ CL
                buf.emit(&[0x0F, 0xB6, 0xC9]); // MOVZX ECX,CL
                buf.emit(&[0x49, 0x01, 0xC8]); // ADD R8,RCX
                add_r64_imm8(buf, R8, 4);
            }
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
/// time, then bytes: neither side is read or written past ECX. Clobbers RAX,
/// RCX, RDX and the flags.
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
    let byte_top = buf.pos();
    buf.emit(&[0x85, 0xC9]); // TEST ECX,ECX
    let done = jcc_patch(buf, 0x84); // JZ
    buf.emit(&[0x0F, 0xB6, 0x02]); // MOVZX EAX,BYTE [RDX]
    buf.emit(&[0x41, 0x88, 0x00]); // MOV [R8],AL
    buf.emit(&[0x48, 0xFF, 0xC2]); // INC RDX
    buf.emit(&[0x49, 0xFF, 0xC0]); // INC R8
    buf.emit(&[0xFF, 0xC9]); // DEC ECX
    let back = jmp_patch(buf);
    patch_back(buf, back, byte_top);
    patch_here(buf, done);
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
            ConcatArgKind::Boolean => {
                buf.emit(&[0x85, 0xC0]); // TEST EAX,EAX
                buf.emit(&[0x0F, 0x94, 0xC1]); // SETZ CL
                buf.emit(&[0x0F, 0xB6, 0xC9]); // MOVZX ECX,CL
                buf.emit(&[0x49, 0x01, 0xC8]); // ADD R8,RCX
                add_r64_imm8(buf, R8, 4);
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
    let shape = ConcatPassShape::new(
        plan,
        outer,
        &alloc,
        compact_flag_addr,
        arg_offsets,
        frame.class_id_off,
    );
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
    emit_zgc_start_bit_precheck_form(buf, cursor_off, end_off, objs, decline, short);
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
    emit_zgc_start_bit_store_prechecked_form(buf, cursor_off, obj, short);
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
// JDK's chain (`--jdk-only`: `StringUTF16.compress` one char per iteration, or
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
    mov_imm64_full(buf, RAX, table as i64); // Cast: the 'static table's address
    cmp_r64_mem(buf, R9, RAX, 0);
    let below_first = jcc_patch(buf, 0x82); // JB
    cmp_r64_mem(buf, R9, RAX, 8);
    let young = jcc_patch(buf, 0x82); // JB: inside the first
    patch_here(buf, below_first);
    cmp_r64_mem(buf, R9, RAX, 16);
    decline.push(jcc_patch(buf, 0x82)); // JB
    cmp_r64_mem(buf, R9, RAX, 24);
    decline.push(jcc_patch(buf, 0x83)); // JAE
    patch_here(buf, young);
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
    // R8 = &value[count] (payload), RAX = i: the decimal text there.
    buf.emit(&[0x4E, 0x8D, 0x44, 0x19, 0x10]); // LEA R8,[RCX+R11+16]
    buf.emit(&[0x49, 0x63, 0xC2]); // MOVSXD RAX,R10D
    emit_decimal(buf); // clobbers RAX, RCX, RDX, R10; R8 past the digits
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
    test_r64_r64(buf, RDX);
    decline.push(jcc_patch(buf, 0x84)); // JZ: no payload
    cmp_mem8_imm8(buf, RDX, i32::from(site.kind_d8), site.byte_array_tag);
    decline.push(jcc_patch(buf, 0x85)); // JNE: not a byte[]
    buf.emit(&[0x85, 0xC9]); // TEST ECX,ECX
    decline.push(jcc_patch(buf, 0x84)); // JZ: an empty builder
    buf.emit(&[0x3B, 0x4A, len_d8]); // CMP ECX,[RDX+len]
    decline.push(jcc_patch(buf, 0x87)); // JA: count past the array
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
    // The payload: `count` bytes of the builder's array (ECX, RDX), then
    // `String.value = the byte[]`.
    lea_r64_mem(buf, RDX, RDX, data);
    lea_r64_mem(buf, R8, R11, data);
    if to_lower {
        emit_copy_latin1_lower(buf); // clobbers RAX, RCX, RDX, R8
    } else {
        emit_copy(buf); // clobbers RAX, RCX, RDX, R8
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
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_CONCAT_INLINE")
        || crate::ir::ir_indy_bridge_with_inline_enabled()
}

/// JIT round 19 proposal wave 5, lane concat5: the shared passes executed over
/// a fake frame and fake Strings (every operand kind, every decline the
/// length pass owns), the optimizing tier's whole tail executed against a fake
/// thread and TLAB, and the collector pre-checks pinned byte for byte to the
/// single-pass `Compiler` forms they copy.
#[cfg(all(test, target_arch = "x86_64"))]
mod r19_concat5_shared_emit_tests {
    use super::*;
    use crate::concat_inline_plan::JitConcatInlinePlan;

    #[cfg(windows)]
    const A0: u8 = 1; // RCX
    #[cfg(windows)]
    const A1: u8 = 2; // RDX
    #[cfg(not(windows))]
    const A0: u8 = 7; // RDI
    #[cfg(not(windows))]
    const A1: u8 = 6; // RSI

    const STRING_CID: u32 = 77;
    const CLASS_ID_OFF: i32 = 0;

    fn layout() -> crate::StringFieldLayout {
        crate::StringFieldLayout {
            value_field_index: 0,
            value_compact_offset: 16,
            value_compact_is_narrow: false,
            value_legacy_offset: 40,
            hash_field_index: 1,
            hash_compact_offset: 28,
            hash_legacy_offset: 56,
            has_coder: true,
            coder_field_index: 2,
            coder_compact_offset: 24,
            coder_legacy_offset: 48,
            coder_compact_is_byte: true,
            string_class_id: STRING_CID,
            builder: None,
            array_list: None,
            hash_is_zero_compact_offset: None,
        }
    }

    fn units(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn plan(recipe: &str, desc: &str) -> &'static JitConcatInlinePlan {
        Box::leak(Box::new(
            JitConcatInlinePlan::from_recipe(&units(recipe), &[], desc).expect("admitted"),
        ))
    }

    /// A fake `byte[]` holding `bytes` (64-bit words, 8-aligned).
    fn byte_array(bytes: &[u8]) -> Vec<u64> {
        let data = cratonvm_types::ARRAY_DATA_OFFSET;
        let mut raw = vec![0u8; data + bytes.len() + 8];
        raw[cratonvm_types::KIND_TAGS_BYTE_OFFSET] =
            cratonvm_types::primitive_array_kind_tags_byte("[B").expect("byte[] tag");
        let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
        // Cast: a small test length.
        raw[len_at..len_at + 4].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
        raw[data..data + bytes.len()].copy_from_slice(bytes);
        words(&raw)
    }

    fn words(raw: &[u8]) -> Vec<u64> {
        let mut w = vec![0u64; raw.len().div_ceil(8)];
        for (i, b) in raw.iter().enumerate() {
            w[i / 8] |= u64::from(*b) << ((i % 8) * 8);
        }
        w
    }

    /// A fake `String` over `value`: compact (offsets 16/24) or legacy
    /// (40/48), `coder`, class `cid`.
    fn string_obj(value: &[u64], coder: u8, compact: bool, cid: u32) -> Vec<u64> {
        let mut raw = vec![0u8; 64];
        raw[0..4].copy_from_slice(&cid.to_le_bytes());
        if compact {
            raw[cratonvm_types::GC_FLAGS_BYTE_OFFSET] = cratonvm_types::GC_FLAG_COMPACT;
        }
        let (v_off, c_off) = if compact { (16, 24) } else { (40, 48) };
        // Cast: the value array's address as a word.
        let addr = value.as_ptr() as u64;
        raw[v_off..v_off + 8].copy_from_slice(&addr.to_le_bytes());
        raw[c_off] = coder;
        words(&raw)
    }

    fn shape(p: &'static JitConcatInlinePlan, flag: &u8, n: usize) -> ConcatPassShape {
        ConcatPassShape {
            plan: p,
            outer: layout(),
            // Cast: the flag's address.
            compact_flag_addr: flag as *const u8 as usize,
            // Cast: small offsets.
            arg_offsets: (0..n).map(|i| 8 * (i as i32 + 1)).collect(),
            class_id_off: CLASS_ID_OFF,
            len_d8: u8::try_from(cratonvm_types::ARRAY_LENGTH_OFFSET).expect("disp8"),
            data_d8: u8::try_from(cratonvm_types::ARRAY_DATA_OFFSET).expect("disp8"),
            max_count: 4096,
            boxed_int: None,
        }
    }

    /// Emit `body` as `extern "C" fn(rbp, a1) -> i64` (RBP = `rbp`, R9 = `a1`
    /// on entry), with every `decline` edge answering `-1`, and run it.
    fn run(body: impl Fn(&mut ExecutableBuffer, &mut Vec<usize>), rbp: u64, a1: u64) -> i64 {
        let mut buf = ExecutableBuffer::new(1 << 16).expect("test buffer");
        buf.emit_byte(0x55); // PUSH RBP
        mov_r64_r64(&mut buf, 5, A0); // MOV RBP, a0
        mov_r64_r64(&mut buf, R9, A1);
        let mut decline = Vec::new();
        body(&mut buf, &mut decline);
        buf.emit(&[0x5D, 0xC3]); // POP RBP ; RET
        for p in decline {
            patch_here(&mut buf, p);
        }
        mov_imm32_sx(&mut buf, RAX, -1);
        buf.emit(&[0x5D, 0xC3]); // POP RBP ; RET
        assert!(!buf.overflowed());
        crate::platform::make_executable(buf.as_ptr() as *mut u8, buf.capacity())
            .expect("the test buffer must be flippable to RX");
        // SAFETY: a leaf snippet over the caller's buffers (alive across the
        // call) and caller-saved registers; RBP is saved and restored.
        let f: extern "C" fn(u64, u64) -> i64 = unsafe { std::mem::transmute(buf.as_ptr()) };
        f(rbp, a1)
    }

    /// The two passes over `args` (frame words, operand 0 first): the text
    /// written, or `None` when the length pass declined. The length pass's
    /// ECX is checked against the bytes the fill wrote.
    fn render(p: &'static JitConcatInlinePlan, args: &[u64], compact_on: bool) -> Option<Vec<u8>> {
        let flag: u8 = u8::from(compact_on);
        let s = shape(p, &flag, args.len());
        // frame[n - 1 - i] = operand i, RBP one past the end: [rbp - 8(i+1)].
        let mut frame = vec![0u64; args.len() + 1];
        for (i, a) in args.iter().enumerate() {
            frame[args.len() - 1 - i] = *a;
        }
        let data = cratonvm_types::ARRAY_DATA_OFFSET;
        let mut dst = vec![0xAAAA_AAAA_AAAA_AAAAu64; 512];
        // Cast: the frame top and the destination array as words.
        let rbp = unsafe { frame.as_ptr().add(args.len()) } as u64;
        let dst_addr = dst.as_mut_ptr() as u64;
        let written = run(
            |b, d| {
                emit_length_pass(b, &s, d);
                // The array "length" := ECX, then the fill into R11 = dst.
                b.emit(&[0x41, 0x89, 0x49, s.len_d8]); // MOV [R9+len],ECX
                mov_r64_r64(b, R11, R9);
                emit_fill_pass(b, &s);
                mov_r64_r64(b, RAX, R8);
                b.emit(&[0x4C, 0x29, 0xD8]); // SUB RAX,R11
            },
            rbp,
            dst_addr,
        );
        if written < 0 {
            return None;
        }
        // Cast: a small non-negative count.
        let n = written as usize - data;
        let raw: Vec<u8> = dst.iter().flat_map(|w| w.to_le_bytes()).collect();
        let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
        let ecx = u32::from_le_bytes(raw[len_at..len_at + 4].try_into().expect("4 bytes"));
        assert_eq!(
            ecx as usize, n,
            "the length pass's count is the bytes the fill wrote"
        );
        assert!(
            raw[data + n..data + n + 16].iter().all(|&b| b == 0xAA),
            "wrote past the result"
        );
        Some(raw[data..data + n].to_vec())
    }

    #[test]
    fn every_operand_kind_renders_as_hotspot_spells_it() {
        let p = plan(
            "i=\u{1} s=\u{1} j=\u{1} c=\u{1} z=\u{1} n=\u{1} f=\u{1}.",
            "(ILjava/lang/String;JCZLjava/lang/String;Z)Ljava/lang/String;",
        );
        let hello = byte_array(b"h\xe9llo world!");
        let s = string_obj(&hello, 0, true, STRING_CID);
        for (i, j) in [
            (0i32, 0i64),
            (-1, -1),
            (i32::MIN, i64::MIN),
            (i32::MAX, i64::MAX),
            (1_000_000_000, 10_000_000_000),
        ] {
            let args = [
                // Cast: an int operand's bits under garbage in the high half.
                (0xDEAD_0000_0000_0000u64) | u64::from(i as u32),
                s.as_ptr() as u64,
                // Cast: the long's bits.
                j as u64,
                0xFFFF_0000_0000_0041u64, // 'A' under garbage
                0x5A5A_0000_0000_0001u64, // true
                0,                        // null
                0x5A5A_0000_0000_0000u64, // false (low word zero)
            ];
            let got = render(p, &args, true).expect("not declined");
            // The result is Latin-1 bytes (the 0xE9 stays one byte).
            let mut want = format!("i={i} s=").into_bytes();
            want.extend_from_slice(b"h\xe9llo world!");
            want.extend(format!(" j={j} c=A z=true n=null f=false.").bytes());
            assert_eq!(got, want, "{i} {j}");
        }
    }

    #[test]
    fn a_legacy_layout_string_is_read_through_its_legacy_offsets() {
        let p = plan("<\u{1}>", "(Ljava/lang/String;)Ljava/lang/String;");
        let v = byte_array(b"legacy");
        let s = string_obj(&v, 0, false, STRING_CID);
        assert_eq!(
            render(p, &[s.as_ptr() as u64], true),
            Some(b"<legacy>".to_vec())
        );
    }

    #[test]
    fn the_length_pass_declines_what_the_fill_cannot_write() {
        let p = plan("x\u{1}", "(Ljava/lang/String;)Ljava/lang/String;");
        let v = byte_array(b"abc");
        // UTF16 coder, a foreign class id, a null value: the bridge.
        let utf16 = string_obj(&v, 1, true, STRING_CID);
        let other = string_obj(&v, 0, true, STRING_CID + 1);
        let no_value = {
            let mut s = string_obj(&v, 0, true, STRING_CID);
            s[2] = 0;
            s
        };
        for (what, s) in [("utf16", &utf16), ("class", &other), ("value", &no_value)] {
            assert_eq!(render(p, &[s.as_ptr() as u64], true), None, "{what}");
        }
        // A non-byte[] value (a char[] tag).
        let mut chars = byte_array(b"abc");
        let tag_word = cratonvm_types::KIND_TAGS_BYTE_OFFSET / 8;
        let tag_shift = (cratonvm_types::KIND_TAGS_BYTE_OFFSET % 8) * 8;
        chars[tag_word] ^= 0x01 << tag_shift;
        let s = string_obj(&chars, 0, true, STRING_CID);
        assert_eq!(render(p, &[s.as_ptr() as u64], true), None, "kind tag");
        // Compact strings off: always the bridge.
        let ok = string_obj(&v, 0, true, STRING_CID);
        assert_eq!(render(p, &[ok.as_ptr() as u64], false), None, "compact off");
        assert_eq!(
            render(p, &[ok.as_ptr() as u64], true),
            Some(b"xabc".to_vec())
        );
        // A char above 0xFF.
        let pc = plan("\u{1}!", "(C)Ljava/lang/String;");
        assert_eq!(render(pc, &[0x100], true), None, "wide char");
        assert_eq!(render(pc, &[0xFF], true), Some(vec![0xFF, b'!']));
        // An empty result (no literal text, an empty String operand).
        let pe = plan("\u{1}", "(Ljava/lang/String;)Ljava/lang/String;");
        let empty = byte_array(b"");
        let es = string_obj(&empty, 0, true, STRING_CID);
        assert_eq!(render(pe, &[es.as_ptr() as u64], true), None, "empty");
    }

    /// The optimizing tier's whole tail, executed: one bump for the `byte[]`
    /// and the String, both headers, the String's zeroed body and `value`, the
    /// text, the commit, and RAX = the String; a full TLAB, a moved replace
    /// count and a null thread decline with the cursor untouched.
    #[test]
    fn the_ir_tail_builds_both_objects_in_one_bump() {
        let p = plan("v=\u{1}/\u{1}", "(ILjava/lang/String;)Ljava/lang/String;");
        let flag: u8 = 1;
        let guard_word: u32 = 5;
        let total = 32i32;
        let value_off = 16i32;
        let run_tail = |tlab_words: usize, guard_value: u32, thread_null: bool| {
            let mut tlab = vec![0u64; tlab_words];
            // A fake thread: cursor at 8, end at 16.
            let mut thread = vec![0u64; 4];
            // Cast: addresses as words.
            let base = tlab.as_mut_ptr() as u64;
            thread[1] = base;
            thread[2] = base + 8 * tlab_words as u64;
            let v = byte_array(b"xyz");
            let s = string_obj(&v, 0, true, STRING_CID);
            let mut frame = vec![0u64; 4];
            // [rbp-8] = operand 0, [rbp-16] = operand 1, [rbp-24] = thread.
            frame[3] = 42;
            frame[2] = s.as_ptr() as u64;
            frame[1] = if thread_null {
                0
            } else {
                thread.as_ptr() as u64
            };
            let rbp = unsafe { frame.as_ptr().add(4) } as u64;
            let site = ConcatIrSite {
                shape: shape(p, &flag, 2),
                alloc: SbToStringPlan::for_test(
                    STRING_CID,
                    total,
                    value_off,
                    (&guard_word as *const u32 as usize, guard_value),
                ),
                gc: SbToStringGc {
                    zgc_start_bits: false,
                    g1_same_region: false,
                },
                thread: ConcatThread::Frame(24),
                cursor_off: 8,
                end_off: 16,
                g1_table: 0,
            };
            let r = run(|b, d| emit_ir_tail(b, &site, d), rbp, 0);
            (r, tlab, thread[1] - base)
        };
        let (r, tlab, advanced) = run_tail(64, 5, false);
        assert!(r > 0, "not declined");
        let raw: Vec<u8> = tlab.iter().flat_map(|w| w.to_le_bytes()).collect();
        let data = cratonvm_types::ARRAY_DATA_OFFSET;
        // The byte[8] "v=42/xyz" at the cursor, then the String at data + 8.
        let text = b"v=42/xyz";
        let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
        assert_eq!(&raw[len_at..len_at + 4], &(text.len() as u32).to_le_bytes());
        assert_eq!(&raw[data..data + text.len()], text);
        let string_at = data + 8;
        // Cast: the answer back to an offset into the TLAB.
        assert_eq!(r as u64 - tlab.as_ptr() as u64, string_at as u64);
        assert_eq!(&raw[string_at..string_at + 4], &STRING_CID.to_le_bytes());
        let flags = raw[string_at + cratonvm_types::GC_FLAGS_BYTE_OFFSET];
        assert_eq!(
            flags,
            cratonvm_types::GC_FLAG_COMPACT | cratonvm_types::GC_FLAG_HEADER
        );
        let vo = string_at + value_off as usize;
        assert_eq!(
            u64::from_le_bytes(raw[vo..vo + 8].try_into().expect("8 bytes")),
            tlab.as_ptr() as u64,
            "String.value is the fresh byte[]"
        );
        assert_eq!(
            advanced,
            (string_at + total as usize) as u64,
            "one bump, committed"
        );
        // A full TLAB, a moved layout and no thread: declined, cursor untouched.
        for (words, guard, null_thread) in [(4usize, 5u32, false), (64, 6, false), (64, 5, true)] {
            let (r, _, advanced) = run_tail(words, guard, null_thread);
            assert_eq!((r, advanced), (-1, 0), "{words} {guard} {null_thread}");
        }
    }

    /// The collector pre-checks are byte for byte the uncounted single-pass
    /// forms they copy, so the two tiers make the collector the same promise.
    #[test]
    fn the_collector_prechecks_match_the_single_pass_bytes() {
        let table = 0x1234_5678usize;
        let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        c.helpers.g1_barrier_addr = table;
        let start = c.buf.pos();
        let mut d = Vec::new();
        c.emit_sb_g1_same_region_check(&mut d, None);
        let want = c.buf.as_slice()[start..].to_vec();
        let mut b = ExecutableBuffer::new(4096).expect("buffer");
        let mut d2 = Vec::new();
        emit_g1_same_region_check(&mut b, table, &mut d2);
        assert_eq!(b.as_slice(), &want[..], "G1 same-region check");

        for (cursor, end) in [(0x40, 0x48), (0x180, 0x188)] {
            let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
            let start = c.buf.pos();
            let mut d = Vec::new();
            c.emit_zgc_start_bit_precheck(cursor, end, &[R11, R9], &mut d, false);
            c.emit_zgc_start_bit_store_prechecked(cursor, R11);
            c.emit_zgc_start_bit_store_prechecked(cursor, R9);
            let want = c.buf.as_slice()[start..].to_vec();
            let mut b = ExecutableBuffer::new(4096).expect("buffer");
            let mut d2 = Vec::new();
            emit_zgc_start_bit_precheck(&mut b, cursor, end, &[R11, R9], &mut d2);
            emit_zgc_start_bit_store_prechecked(&mut b, cursor, R11);
            emit_zgc_start_bit_store_prechecked(&mut b, cursor, R9);
            assert_eq!(b.as_slice(), &want[..], "ZGC start bits at {cursor:#x}");
            assert_eq!(d.len(), d2.len());
        }
    }

    /// `"" + b` is the one unary shape HotSpot answers with a literal: the IR
    /// twin declines it; a boolean beside literal text, or a unary `int`,
    /// stays admitted.
    #[test]
    fn the_unary_boolean_concat_keeps_the_bridge() {
        let frame = ConcatIrFrame {
            tls_disp: 0,
            thread_slot_off: 24,
            cursor_off: 8,
            end_off: 16,
            class_id_off: 0,
            g1_table: 0,
            region_bounds_addr: 0,
        };
        let unary = plan("\u{1}", "(Z)Ljava/lang/String;");
        assert!(is_unary_boolean(unary));
        let why = concat_ir_site(Some(unary), Some(layout()), Some(1), vec![8], frame)
            .expect_err("declined");
        assert!(why.contains("unary boolean"), "{why}");
        assert!(!is_unary_boolean(plan("b=\u{1}", "(Z)Ljava/lang/String;")));
        assert!(!is_unary_boolean(plan("\u{1}", "(I)Ljava/lang/String;")));
        assert!(!is_unary_boolean(plan(
            "\u{1}\u{1}",
            "(ZZ)Ljava/lang/String;"
        )));
    }

    /// UTF-16LE bytes of a Rust string (the expected result payload).
    fn u16le(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
    }

    /// STRGAP4-4: a wide plan's two passes write UTF-16LE units with the same
    /// spellings as the Latin-1 passes -- a LATIN1 String inflated, a UTF16
    /// String copied, a `char` above 0xFF inline, `null`, booleans, the int
    /// edges -- and the length pass's ECX is the byte count the fill wrote.
    #[test]
    fn the_wide_passes_render_utf16_as_hotspot_spells_it() {
        let p = plan(
            "\u{3a9}=\u{1} s=\u{1} w=\u{1} c=\u{1} z=\u{1} n=\u{1}.",
            "(ILjava/lang/String;Ljava/lang/String;CZLjava/lang/String;)Ljava/lang/String;",
        );
        assert!(p.wide);
        let latin = byte_array(b"h\xe9llo world!");
        let ls = string_obj(&latin, 0, true, STRING_CID);
        let wide_bytes = u16le("\u{3a9}mega\u{4e2d}");
        let wide = byte_array(&wide_bytes);
        let ws = string_obj(&wide, 1, true, STRING_CID);
        let legacy_wide = string_obj(&wide, 1, false, STRING_CID);
        for (i, w) in [
            (0i32, &ws),
            (-1, &legacy_wide),
            (i32::MIN, &ws),
            (i32::MAX, &ws),
            (1_000_000_000, &legacy_wide),
        ] {
            let args = [
                // Cast: an int operand's bits under garbage in the high half.
                0xDEAD_0000_0000_0000u64 | u64::from(i as u32),
                ls.as_ptr() as u64,
                w.as_ptr() as u64,
                0xFFFF_0000_0000_03A9u64, // the char U+03A9 under garbage
                0x5A5A_0000_0000_0001u64, // true
                0,                        // null
            ];
            let got = render(p, &args, true).expect("not declined");
            let want = u16le(&format!(
                "\u{3a9}={i} s=h\u{e9}llo world! w=\u{3a9}mega\u{4e2d} c=\u{3a9} z=true n=null."
            ));
            assert_eq!(got, want, "{i}");
        }
        // A long operand's edges and a false boolean.
        let pl = plan("\u{3a9}\u{1}|\u{1}", "(JZ)Ljava/lang/String;");
        for j in [0i64, -7, i64::MIN, i64::MAX] {
            // Cast: the long's bits.
            let got = render(pl, &[j as u64, 0x5A5A_0000_0000_0000], true).expect("not declined");
            assert_eq!(got, u16le(&format!("\u{3a9}{j}|false")), "{j}");
        }
    }

    /// STRGAP4-4: what the wide length pass still refuses, before any store:
    /// compact strings off, a coder that is neither LATIN1 nor UTF16, a
    /// foreign class, a null value, a result past the cap (in units).
    #[test]
    fn the_wide_length_pass_declines_what_it_cannot_write() {
        let p = plan("\u{3a9}\u{1}", "(Ljava/lang/String;)Ljava/lang/String;");
        let v = byte_array(b"abcd");
        let ok = string_obj(&v, 0, true, STRING_CID);
        assert_eq!(render(p, &[ok.as_ptr() as u64], false), None, "compact off");
        let bad_coder = string_obj(&v, 2, true, STRING_CID);
        let other = string_obj(&v, 0, true, STRING_CID + 1);
        let no_value = {
            let mut s = string_obj(&v, 0, true, STRING_CID);
            s[2] = 0;
            s
        };
        for (what, s) in [
            ("coder", &bad_coder),
            ("class", &other),
            ("value", &no_value),
        ] {
            assert_eq!(render(p, &[s.as_ptr() as u64], true), None, "{what}");
        }
        assert_eq!(
            render(p, &[ok.as_ptr() as u64], true),
            Some(u16le("\u{3a9}abcd"))
        );
        // 2049 units are 4098 bytes, past the 4096-byte cap (declined before
        // the fill, so the test's 4 KiB destination is never written).
        let over = byte_array(&vec![b'a'; 2048]);
        let os = string_obj(&over, 0, true, STRING_CID);
        assert_eq!(render(p, &[os.as_ptr() as u64], true), None, "past the cap");
    }

    /// STRGAP4-4: the optimizing tier's tail of a wide plan stores
    /// `coder = UTF16` and sizes the `byte[]` at two bytes per unit; a wide
    /// plan without `CRATONVM_JIT_CONCAT_UTF16` keeps the bridge.
    #[test]
    fn the_ir_tail_of_a_wide_plan_builds_a_utf16_string() {
        let p = plan("\u{3a9}=\u{1}", "(I)Ljava/lang/String;");
        let flag: u8 = 1;
        let guard_word: u32 = 5;
        let total = 32i32;
        let value_off = 16i32;
        let mut tlab = vec![0u64; 64];
        let mut thread = vec![0u64; 4];
        // Cast: addresses as words.
        let base = tlab.as_mut_ptr() as u64;
        thread[1] = base;
        thread[2] = base + 8 * 64;
        let mut frame = vec![0u64; 4];
        // [rbp-8] = operand 0, [rbp-24] = thread.
        frame[3] = 42;
        frame[1] = thread.as_ptr() as u64;
        let rbp = unsafe { frame.as_ptr().add(4) } as u64;
        let mut alloc = SbToStringPlan::for_test(
            STRING_CID,
            total,
            value_off,
            (&guard_word as *const u32 as usize, 5),
        );
        alloc.utf16_coder = Some((24, true));
        let site = ConcatIrSite {
            shape: shape(p, &flag, 1),
            alloc,
            gc: SbToStringGc {
                zgc_start_bits: false,
                g1_same_region: false,
            },
            thread: ConcatThread::Frame(24),
            cursor_off: 8,
            end_off: 16,
            g1_table: 0,
        };
        let r = run(|b, d| emit_ir_tail(b, &site, d), rbp, 0);
        assert!(r > 0, "not declined");
        let raw: Vec<u8> = tlab.iter().flat_map(|w| w.to_le_bytes()).collect();
        let data = cratonvm_types::ARRAY_DATA_OFFSET;
        let text = u16le("\u{3a9}=42");
        let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
        assert_eq!(&raw[len_at..len_at + 4], &(text.len() as u32).to_le_bytes());
        assert_eq!(&raw[data..data + text.len()], &text[..]);
        let string_at = data + text.len().div_ceil(8) * 8;
        // Cast: the answer back to an offset into the TLAB.
        assert_eq!(r as u64 - tlab.as_ptr() as u64, string_at as u64);
        assert_eq!(raw[string_at + 24], 1, "coder = UTF16");
        assert_eq!(thread[1] - base, (string_at + total as usize) as u64);

        let frame_facts = ConcatIrFrame {
            tls_disp: 0,
            thread_slot_off: 24,
            cursor_off: 8,
            end_off: 16,
            class_id_off: 0,
            g1_table: 0,
            region_bounds_addr: 0,
        };
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_CONCAT_UTF16", None)],
            || {
                let why = concat_ir_site(Some(p), Some(layout()), Some(1), vec![8], frame_facts)
                    .expect_err("declined");
                assert!(why.contains("CRATONVM_JIT_CONCAT_UTF16"), "{why}");
            },
        );
    }

    #[test]
    fn the_ir_switch_is_off_by_default() {
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_IR_CONCAT_INLINE", None),
                ("CRATONVM_JIT_IR_INDY_BRIDGE_WITH_INLINE", None),
            ],
            || assert!(!ir_concat_inline_enabled()),
        );
        // The paired switch turns the IR inline and the IR bridge on together.
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_IR_CONCAT_INLINE", None),
                ("CRATONVM_JIT_IR_INDY_BRIDGE", None),
                ("CRATONVM_JIT_IR_INDY_BRIDGE_WITH_INLINE", Some("1")),
            ],
            || {
                assert!(ir_concat_inline_enabled());
                assert!(crate::ir::ir_indy_bridge_enabled());
            },
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_CONCAT_INLINE", Some("1"))],
            || assert!(ir_concat_inline_enabled()),
        );
    }

    /// The two passes over `args` with an optional CONCAT21-4 boxed-`Integer`
    /// shape and the CONCAT21-3 FP prologue / restore around them (every
    /// prologue decline through a restore stub, as both tiers wire it). R9 is
    /// re-derived after the prologue: its leaf call clobbers it.
    fn render_full(
        p: &'static JitConcatInlinePlan,
        args: &[u64],
        boxed: Option<ConcatBoxedInt>,
    ) -> Option<Vec<u8>> {
        let flag: u8 = 1;
        let mut s = shape(p, &flag, args.len());
        s.boxed_int = boxed;
        let mut frame = vec![0u64; args.len() + 1];
        for (i, a) in args.iter().enumerate() {
            frame[args.len() - 1 - i] = *a;
        }
        let data = cratonvm_types::ARRAY_DATA_OFFSET;
        let mut dst = vec![0xAAAA_AAAA_AAAA_AAAAu64; 512];
        // Cast: the frame top and the destination array as words.
        let rbp = unsafe { frame.as_ptr().add(args.len()) } as u64;
        let dst_addr = dst.as_mut_ptr() as u64;
        let written = run(
            |b, d| {
                let mut inner = Vec::new();
                emit_fp_prologue(b, &s, &mut inner);
                // Cast: the destination's address as an imm64.
                mov_imm64_full(b, R9, dst_addr as i64);
                emit_length_pass(b, &s, &mut inner);
                b.emit(&[0x41, 0x89, 0x49, s.len_d8]); // MOV [R9+len],ECX
                mov_r64_r64(b, R11, R9);
                emit_fill_pass(b, &s);
                mov_r64_r64(b, RAX, R8);
                b.emit(&[0x4C, 0x29, 0xD8]); // SUB RAX,R11
                emit_fp_restore(b, &s);
                let done = jmp_patch(b);
                for e in inner {
                    patch_here(b, e);
                }
                emit_fp_restore(b, &s);
                d.push(jmp_patch(b));
                patch_here(b, done);
            },
            rbp,
            dst_addr,
        );
        if written < 0 {
            return None;
        }
        // Cast: a small non-negative count.
        let n = written as usize - data;
        let raw: Vec<u8> = dst.iter().flat_map(|w| w.to_le_bytes()).collect();
        let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
        let ecx = u32::from_le_bytes(raw[len_at..len_at + 4].try_into().expect("4 bytes"));
        assert_eq!(ecx as usize, n, "the length pass's count is the bytes written");
        assert!(
            raw[data + n..data + n + 16].iter().all(|&b| b == 0xAA),
            "wrote past the result"
        );
        Some(raw[data..data + n].to_vec())
    }

    /// CONCAT21-3 (`CRATONVM_JIT_CONCAT_FP_INLINE`): float / double operands
    /// render through the leaf into the scratch area and are copied (LATIN1)
    /// or inflated (wide) as the shared formatter spells them; RSP comes back
    /// (the harness's own `RET` proves it) on the hit path.
    #[test]
    fn r21_fp_operands_render_through_the_leaf() {
        let p = plan("f=\u{1} d=\u{1} i=\u{1};", "(FDI)Ljava/lang/String;");
        assert!(p.has_fp());
        for (f, dv) in [
            (1.5f32, 1.0e-5f64),
            (f32::NAN, f64::NEG_INFINITY),
            (-0.0, 0.1 + 0.2),
            (1e10, f64::MIN_POSITIVE),
            (f32::MIN_POSITIVE, 5e-324),
        ] {
            let args = [
                // A float is the slot's low 32 bits, under garbage.
                0x5A5A_5A5A_0000_0000u64 | u64::from(f.to_bits()),
                dv.to_bits(),
                0xDEAD_0000_FFFF_FFF9u64, // int -7 under garbage
            ];
            let want = format!(
                "f={} d={} i=-7;",
                cratonvm_types::java_float_to_string(f),
                cratonvm_types::java_double_to_string(dv)
            );
            assert_eq!(render_full(p, &args, None), Some(want.into_bytes()), "{f} {dv}");
        }
        // The wide passes inflate the same text.
        let pw = plan("\u{3a9}\u{1}", "(D)Ljava/lang/String;");
        assert!(pw.wide && pw.has_fp());
        assert_eq!(
            render_full(pw, &[1e7f64.to_bits()], None),
            Some(u16le("\u{3a9}1.0E7"))
        );
    }

    /// CONCAT21-4 (`CRATONVM_JIT_CONCAT_VALUE_OF_FUSION`): a `String` operand
    /// holding an exact Integer renders its `int` (compact and legacy
    /// layouts, both passes, the wide twin too); a real String still copies,
    /// an `Integer[]` (same class id, an array kind tag) declines, and
    /// without the shape any non-String declines as before.
    #[test]
    fn r21_a_fused_integer_operand_renders_its_int() {
        const INT_CID: u32 = 91;
        let boxed = ConcatBoxedInt {
            class_id: INT_CID,
            compact_off: 16,
            legacy_off: 40,
        };
        let integer = |v: i32, compact: bool| -> Vec<u64> {
            let mut raw = vec![0u8; 64];
            raw[0..4].copy_from_slice(&INT_CID.to_le_bytes());
            if compact {
                raw[cratonvm_types::GC_FLAGS_BYTE_OFFSET] = cratonvm_types::GC_FLAG_COMPACT;
            }
            let at = if compact { 16 } else { 40 };
            raw[at..at + 4].copy_from_slice(&v.to_le_bytes());
            words(&raw)
        };
        let p = plan("<\u{1}|\u{1}>", "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;");
        let hello = byte_array(b"hi");
        let s = string_obj(&hello, 0, true, STRING_CID);
        for (v, compact) in [(42, true), (-7, false), (i32::MIN, true), (0, false)] {
            let i = integer(v, compact);
            assert_eq!(
                render_full(p, &[i.as_ptr() as u64, s.as_ptr() as u64], Some(boxed)),
                Some(format!("<{v}|hi>").into_bytes()),
                "{v} {compact}"
            );
        }
        // Without the shape: the bridge, as before.
        let i = integer(5, true);
        assert_eq!(render_full(p, &[i.as_ptr() as u64, s.as_ptr() as u64], None), None);
        // An Integer[] carries the component's id: declines.
        let mut arr = integer(5, true);
        let tag_word = cratonvm_types::KIND_TAGS_BYTE_OFFSET / 8;
        let tag_shift = (cratonvm_types::KIND_TAGS_BYTE_OFFSET % 8) * 8;
        arr[tag_word] |= u64::from(cratonvm_types::KIND_TAG_BYTE_MASK) << tag_shift;
        assert_eq!(
            render_full(p, &[arr.as_ptr() as u64, s.as_ptr() as u64], Some(boxed)),
            None
        );
        // The wide passes render it as units.
        let pw = plan("\u{3a9}\u{1}", "(Ljava/lang/String;)Ljava/lang/String;");
        let i = integer(-12, true);
        assert_eq!(
            render_full(pw, &[i.as_ptr() as u64], Some(boxed)),
            Some(u16le("\u{3a9}-12"))
        );
    }

    /// The marker the promoted (wide) build adds to its written count.
    const PROMOTED: i64 = 1 << 40;

    /// CONCAT21-1: the LATIN1 passes with their UTF16 edges on the wide
    /// twin's passes, executed as both tiers wire them: `Some((text, wide))`,
    /// `wide` when the twin built it, `None` when either build declined.
    fn render_promoting(
        p: &'static JitConcatInlinePlan,
        args: &[u64],
    ) -> Option<(Vec<u8>, bool)> {
        let twin = p.promotion_twin().expect("a twin");
        let flag: u8 = 1;
        let s = shape(p, &flag, args.len());
        let ws = shape(twin, &flag, args.len());
        let mut frame = vec![0u64; args.len() + 1];
        for (i, a) in args.iter().enumerate() {
            frame[args.len() - 1 - i] = *a;
        }
        let data = cratonvm_types::ARRAY_DATA_OFFSET;
        let mut dst = vec![0xAAAA_AAAA_AAAA_AAAAu64; 512];
        // Cast: the frame top and the destination array as words.
        let rbp = unsafe { frame.as_ptr().add(args.len()) } as u64;
        let dst_addr = dst.as_mut_ptr() as u64;
        let build = |b: &mut ExecutableBuffer, sh: &ConcatPassShape| {
            b.emit(&[0x41, 0x89, 0x49, sh.len_d8]); // MOV [R9+len],ECX
            mov_r64_r64(b, R11, R9);
            emit_fill_pass(b, sh);
            mov_r64_r64(b, RAX, R8);
            b.emit(&[0x4C, 0x29, 0xD8]); // SUB RAX,R11
        };
        let written = run(
            |b, d| {
                let mut promote = Vec::new();
                emit_length_pass_promoting(b, &s, d, &mut promote);
                build(b, &s);
                let done = jmp_patch(b);
                assert!(!promote.is_empty(), "a String / char operand has a UTF16 edge");
                for e in promote {
                    patch_here(b, e);
                }
                emit_length_pass(b, &ws, d);
                build(b, &ws);
                mov_imm64_full(b, R10, PROMOTED);
                b.emit(&[0x4C, 0x01, 0xD0]); // ADD RAX,R10
                patch_here(b, done);
            },
            rbp,
            dst_addr,
        );
        if written < 0 {
            return None;
        }
        let wide = written & PROMOTED != 0;
        // Cast: a small non-negative count.
        let n = (written & !PROMOTED) as usize - data;
        let raw: Vec<u8> = dst.iter().flat_map(|w| w.to_le_bytes()).collect();
        let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
        let ecx = u32::from_le_bytes(raw[len_at..len_at + 4].try_into().expect("4 bytes"));
        assert_eq!(ecx as usize, n, "the length pass's count is the bytes written");
        assert!(
            raw[data + n..data + n + 16].iter().all(|&b| b == 0xAA),
            "wrote past the result"
        );
        Some((raw[data..data + n].to_vec(), wide))
    }

    /// CONCAT21-1 (`CRATONVM_JIT_CONCAT_CODER_PROMOTE`): a LATIN1 site whose
    /// operand is UTF16 at run time -- a `char` above `0xFF`, a UTF16 String,
    /// anywhere in the chain -- is built by the wide twin as UTF-16LE (the
    /// LATIN1 operands before it inflated), exactly HotSpot's text; LATIN1
    /// operands keep the LATIN1 build; an empty twin result and every other
    /// refusal still decline to the bridge.
    #[test]
    fn r21_a_utf16_operand_promotes_to_the_wide_twin() {
        let pc = plan("x=\u{1};", "(C)Ljava/lang/String;");
        assert_eq!(render_promoting(pc, &[0x41]), Some((b"x=A;".to_vec(), false)));
        assert_eq!(
            render_promoting(pc, &[0xFFFF_0000_0000_03A9]),
            Some((u16le("x=\u{3a9};"), true))
        );
        assert_eq!(render_promoting(pc, &[0xFF]), Some((vec![b'x', b'=', 0xFF, b';'], false)));
        // A UTF16 String after a LATIN1 one and an int: the chain promotes
        // at its third operand, the first two re-rendered as units.
        let p = plan(
            "\u{1}+\u{1}+\u{1}\u{e9}",
            "(Ljava/lang/String;ILjava/lang/String;)Ljava/lang/String;",
        );
        let latin = byte_array(b"ab");
        let ls = string_obj(&latin, 0, true, STRING_CID);
        let wbytes = u16le("\u{3a9}z");
        let wv = byte_array(&wbytes);
        let ws = string_obj(&wv, 1, true, STRING_CID);
        assert_eq!(
            render_promoting(p, &[ls.as_ptr() as u64, 7, ws.as_ptr() as u64]),
            Some((u16le("ab+7+\u{3a9}z\u{e9}"), true))
        );
        assert_eq!(
            render_promoting(p, &[ls.as_ptr() as u64, 7, ls.as_ptr() as u64]),
            Some((b"ab+7+ab\xe9".to_vec(), false))
        );
        // null stays "null" on both builds.
        assert_eq!(
            render_promoting(p, &[0, 7, ws.as_ptr() as u64]),
            Some((u16le("null+7+\u{3a9}z\u{e9}"), true))
        );
        // `s1 + s2` has no literal text: an empty twin result declines.
        let pe = plan("\u{1}\u{1}", "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;");
        let empty = byte_array(b"");
        let ew = string_obj(&empty, 1, true, STRING_CID);
        assert_eq!(render_promoting(pe, &[ew.as_ptr() as u64, ew.as_ptr() as u64]), None);
        assert_eq!(
            render_promoting(pe, &[ls.as_ptr() as u64, ws.as_ptr() as u64]),
            Some((u16le("ab\u{3a9}z"), true))
        );
        // A foreign class still declines (the twin re-screens it).
        let other = string_obj(&wv, 1, true, STRING_CID + 1);
        assert_eq!(render_promoting(pe, &[ls.as_ptr() as u64, other.as_ptr() as u64]), None);
    }
}

/// JIT r20 wave 12, lane qStr (W6-L2-2): the `char[]` copy tails
/// ([`emit_ir_to_char_array`], [`emit_ir_string_from_chars`]) EXECUTED against
/// a fake thread and TLAB, on both arms of `CRATONVM_JIT_IR_STRING_SSE2`: the
/// objects they build, the coder they choose (the JDK's `COMPACT_STRINGS` +
/// `StringUTF16.compress` answer) and every decline, which must leave the
/// cursor where it was.
#[cfg(all(test, target_arch = "x86_64"))]
mod r20_qstr_chars_tests {
    use super::*;

    #[cfg(windows)]
    const A0: u8 = 1; // RCX
    #[cfg(windows)]
    const A1: u8 = 2; // RDX
    #[cfg(not(windows))]
    const A0: u8 = 7; // RDI
    #[cfg(not(windows))]
    const A1: u8 = 6; // RSI

    const STRING_CID: u32 = 77;
    const TOTAL: i32 = 32;
    const VALUE_OFF: i32 = 16;
    const CODER: (i32, bool) = (24, true);
    const SSE_ARMS: [Option<&str>; 2] = [None, Some("0")];

    fn words(raw: &[u8]) -> Vec<u64> {
        let mut w = vec![0u64; raw.len().div_ceil(8)];
        for (i, b) in raw.iter().enumerate() {
            w[i / 8] |= u64::from(*b) << ((i % 8) * 8);
        }
        w
    }

    fn raw(w: &[u64]) -> Vec<u8> {
        w.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    fn u16le(units: &[u16]) -> Vec<u8> {
        units.iter().flat_map(|u| u.to_le_bytes()).collect()
    }

    /// A fake primitive array of kind `desc` with header length `len` and
    /// payload `payload`.
    fn array(desc: &str, len: u32, payload: &[u8]) -> Vec<u64> {
        let data = cratonvm_types::ARRAY_DATA_OFFSET;
        let mut r = vec![0u8; data + payload.len() + 8];
        r[cratonvm_types::KIND_TAGS_BYTE_OFFSET] =
            cratonvm_types::primitive_array_kind_tags_byte(desc).expect("array tag");
        let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
        r[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
        r[data..data + payload.len()].copy_from_slice(payload);
        words(&r)
    }

    fn char_mark() -> i32 {
        use cratonvm_types::{ArrayElementType, ClassId, ObjectHeader, ObjectKind};
        let m = ObjectHeader::new(
            ClassId::new(0),
            ObjectKind::Array,
            ArrayElementType::Char,
            0,
            0,
        )
        .mark_word
        .load(std::sync::atomic::Ordering::Relaxed);
        i32::from_le_bytes(m.to_le_bytes())
    }

    fn site(flag: &u8, guard: &u32, guard_value: u32) -> CharsIrSite {
        CharsIrSite {
            alloc: SbToStringPlan::for_test(
                STRING_CID,
                TOTAL,
                VALUE_OFF,
                // Cast: the guard word's address.
                (guard as *const u32 as usize, guard_value),
            ),
            gc: SbToStringGc {
                zgc_start_bits: false,
                g1_same_region: false,
            },
            thread: ConcatThread::Frame(8),
            cursor_off: 8,
            end_off: 16,
            g1_table: 0,
            class_id_off: 0,
            // Cast: the flag's address.
            compact_flag_addr: flag as *const u8 as usize,
            young_bounds_addr: 0,
            coder: CODER,
            kind_d8: u8::try_from(cratonvm_types::KIND_TAGS_BYTE_OFFSET).expect("disp8"),
            byte_array_tag: cratonvm_types::primitive_array_kind_tags_byte("[B").expect("[B"),
            char_array_tag: cratonvm_types::primitive_array_kind_tags_byte("[C").expect("[C"),
            char_array_mark: char_mark(),
        }
    }

    /// Emit `body` as `extern "C" fn(rbp, a1) -> i64` (RBP = `rbp`, R9 = `a1`
    /// on entry), every `decline` edge answering `-1`, and run it.
    fn run(body: impl Fn(&mut ExecutableBuffer, &mut Vec<usize>), rbp: u64, a1: u64) -> i64 {
        let mut buf = ExecutableBuffer::new(1 << 16).expect("test buffer");
        buf.emit_byte(0x55); // PUSH RBP
        mov_r64_r64(&mut buf, 5, A0); // MOV RBP, a0
        mov_r64_r64(&mut buf, R9, A1);
        let mut decline = Vec::new();
        body(&mut buf, &mut decline);
        buf.emit(&[0x5D, 0xC3]); // POP RBP ; RET
        for p in decline {
            patch_here(&mut buf, p);
        }
        mov_imm32_sx(&mut buf, RAX, -1);
        buf.emit(&[0x5D, 0xC3]); // POP RBP ; RET
        assert!(!buf.overflowed());
        crate::platform::make_executable(buf.as_ptr() as *mut u8, buf.capacity())
            .expect("the test buffer must be flippable to RX");
        // SAFETY: a leaf snippet over the caller's buffers (alive across the
        // call) and caller-saved registers; RBP is saved and restored.
        let f: extern "C" fn(u64, u64) -> i64 = unsafe { std::mem::transmute(buf.as_ptr()) };
        f(rbp, a1)
    }

    /// A TLAB of `tlab_words`, a thread over it, and a frame whose `[rbp-8]`
    /// is that thread; `go(rbp)` runs the tail. Answers the tail's RAX, the
    /// TLAB's bytes, the TLAB's base and how far the cursor moved.
    fn with_tlab(tlab_words: usize, go: impl Fn(u64) -> i64) -> (i64, Vec<u8>, u64, u64) {
        let mut tlab = vec![0u64; tlab_words];
        // Cast: addresses as words.
        let base = tlab.as_mut_ptr() as u64;
        let mut thread = vec![0u64; 4];
        thread[1] = base;
        thread[2] = base + 8 * tlab_words as u64; // Cast: a small count
                                                  // Cast: the thread's address as the frame word.
        let frame = [thread.as_ptr() as u64, 0];
        // SAFETY: one past `frame[0]`, inside the array.
        let rbp = unsafe { frame.as_ptr().add(1) } as u64;
        let r = go(rbp);
        let moved = thread[1] - base;
        (r, raw(&tlab), base, moved)
    }

    fn header_u32(raw: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(raw[at..at + 4].try_into().expect("4 bytes"))
    }

    #[test]
    fn to_char_array_inflates_latin1_and_copies_utf16() {
        let flag: u8 = 1;
        let guard: u32 = 5;
        let s = site(&flag, &guard, 5);
        let data = cratonvm_types::ARRAY_DATA_OFFSET;
        let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
        let mark_at = cratonvm_types::MARK_WORD_OFFSET;
        let kind_at = cratonvm_types::KIND_TAGS_BYTE_OFFSET;
        let c_tag = cratonvm_types::primitive_array_kind_tags_byte("[C").expect("[C");
        let text: &[u8] = b"h\xe9llo, world -- 0123456789";
        let wide: Vec<u16> = vec![
            0x41, 0x3A9, 0x42, 0xFFFF, 0x00E9, 0x20AC, 0x43, 0x44, 0x45, 0xD800,
        ];
        let latin_units: Vec<u16> = text.iter().map(|&b| u16::from(b)).collect();
        // Cast: small counts.
        let cases: [(Vec<u64>, u8, Vec<u16>); 3] = [
            (array("[B", text.len() as u32, text), 0, latin_units),
            (
                array("[B", 2 * wide.len() as u32, &u16le(&wide)),
                1,
                wide.clone(),
            ),
            (array("[B", 0, b""), 0, Vec::new()),
        ];
        for sse in SSE_ARMS {
            for (value, coder, want) in &cases {
                let (r, t, base, moved) = with_tlab(64, |rbp| {
                    cratonvm_types::flags::with_thread_overrides(
                        &[("CRATONVM_JIT_IR_STRING_SSE2", sse)],
                        || {
                            run(
                                |b, d| {
                                    // Cast: the fake array's address.
                                    mov_imm64_full(b, RDX, value.as_ptr() as i64);
                                    mov_imm32_sx(b, RCX, i32::from(*coder));
                                    emit_ir_to_char_array(b, &s, d);
                                },
                                rbp,
                                0,
                            )
                        },
                    )
                });
                // Cast: the answer as an address.
                assert_eq!(
                    r as u64, base,
                    "sse={sse:?} coder={coder}: the array at the cursor"
                );
                let n = want.len();
                assert_eq!(header_u32(&t, len_at) as usize, n, "the char[]'s length");
                assert_eq!(
                    header_u32(&t, mark_at),
                    u32::from_le_bytes(char_mark().to_le_bytes())
                );
                assert_eq!(t[kind_at], c_tag, "a char[] by its kind tag");
                let got: Vec<u16> = t[data..data + 2 * n]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                assert_eq!(&got, want, "sse={sse:?} coder={coder}");
                // Cast: a small offset.
                assert_eq!(
                    moved as usize,
                    data + (2 * n).div_ceil(8) * 8,
                    "one bump, committed"
                );
            }
        }
        // Declines: a null value, a char[] value, a coder of 2, an odd UTF16
        // length, a full TLAB -- each with the cursor untouched.
        let odd = array("[B", 3, b"abc");
        let not_bytes = array("[C", 3, b"a\0b\0c\0");
        let ok = array("[B", 3, b"abc");
        // Cast: the fake arrays' addresses.
        let declines: [(u64, u8, usize, &str); 5] = [
            (0, 0, 64, "null value"),
            (not_bytes.as_ptr() as u64, 0, 64, "char[] value"),
            (ok.as_ptr() as u64, 2, 64, "coder 2"),
            (odd.as_ptr() as u64, 1, 64, "odd UTF16 length"),
            (ok.as_ptr() as u64, 0, 2, "full TLAB"),
        ];
        for (value, coder, tlab_words, what) in declines {
            let (r, _, _, moved) = with_tlab(tlab_words, |rbp| {
                run(
                    |b, d| {
                        // Cast: the fake array's address.
                        mov_imm64_full(b, RDX, value as i64);
                        mov_imm32_sx(b, RCX, i32::from(coder));
                        emit_ir_to_char_array(b, &s, d);
                    },
                    rbp,
                    0,
                )
            });
            assert_eq!((r, moved), (-1, 0), "{what}");
        }
    }

    /// `valueOf(char[])`: LATIN1 when compact strings are on and every char
    /// fits (compressed), UTF16 otherwise (copied), as JDK 25 chooses.
    #[test]
    fn value_of_chars_builds_a_string_with_the_jdk_coder() {
        let guard: u32 = 5;
        let data = cratonvm_types::ARRAY_DATA_OFFSET;
        let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
        let latin: Vec<u16> = b"hello, w\xe9rld -- 0123"
            .iter()
            .map(|&b| u16::from(b))
            .collect();
        let mut one_wide = latin.clone();
        one_wide[13] = 0x3A9;
        let mut last_wide = latin.clone();
        last_wide[19] = 0x100;
        let cases: [(&Vec<u16>, u8, bool); 4] = [
            (&latin, 1, false),
            (&one_wide, 1, true),
            (&last_wide, 1, true),
            (&latin, 0, true),
        ];
        for sse in SSE_ARMS {
            for (units, compact, want_utf16) in cases {
                let flag = compact;
                let s = site(&flag, &guard, 5);
                // Cast: a small count.
                let chars = array("[C", units.len() as u32, &u16le(units));
                let (r, t, base, moved) = with_tlab(64, |rbp| {
                    cratonvm_types::flags::with_thread_overrides(
                        &[("CRATONVM_JIT_IR_STRING_SSE2", sse)],
                        || {
                            run(
                                |b, d| {
                                    // Cast: the fake array's address.
                                    mov_imm64_full(b, RDX, chars.as_ptr() as i64);
                                    emit_ir_string_from_chars(b, &s, d, false);
                                },
                                rbp,
                                0,
                            )
                        },
                    )
                });
                assert_ne!(r, -1, "sse={sse:?} compact={compact}: not declined");
                let n = units.len();
                let blen = if want_utf16 { 2 * n } else { n };
                assert_eq!(header_u32(&t, len_at) as usize, blen, "the byte[]'s length");
                let payload = &t[data..data + blen];
                if want_utf16 {
                    assert_eq!(payload, &u16le(units)[..], "sse={sse:?} UTF16 copy");
                } else {
                    let low: Vec<u8> = units.iter().map(|&u| u.to_le_bytes()[0]).collect();
                    assert_eq!(payload, &low[..], "sse={sse:?} LATIN1 compress");
                }
                let string_at = data + blen.div_ceil(8) * 8;
                // Cast: the answer back to an offset into the TLAB.
                assert_eq!(r as u64 - base, string_at as u64, "RAX = the String");
                assert_eq!(header_u32(&t, string_at), STRING_CID);
                let vo = string_at + VALUE_OFF as usize; // Cast: a small offset
                assert_eq!(
                    u64::from_le_bytes(t[vo..vo + 8].try_into().expect("8 bytes")),
                    base,
                    "String.value is the fresh byte[]"
                );
                let co = string_at + CODER.0 as usize; // Cast: a small offset
                assert_eq!(t[co], u8::from(want_utf16), "coder");
                // Cast: small offsets.
                assert_eq!(moved as usize, string_at + TOTAL as usize, "one bump");
            }
        }
        // Declines: null, an empty array (the JDK's shared "".value), a
        // byte[] argument, a replaced String layout, a full TLAB.
        let flag: u8 = 1;
        let empty = array("[C", 0, b"");
        let bytes = array("[B", 2, b"ab");
        let ok = array("[C", 2, &u16le(&[0x61, 0x62]));
        // Cast: the fake arrays' addresses.
        let declines: [(u64, u32, usize, &str); 5] = [
            (0, 5, 64, "null"),
            (empty.as_ptr() as u64, 5, 64, "empty"),
            (bytes.as_ptr() as u64, 5, 64, "byte[]"),
            (ok.as_ptr() as u64, 6, 64, "replaced layout"),
            (ok.as_ptr() as u64, 5, 4, "full TLAB"),
        ];
        for (arg, guard_value, tlab_words, what) in declines {
            let s = site(&flag, &guard, guard_value);
            let (r, _, _, moved) = with_tlab(tlab_words, |rbp| {
                run(
                    |b, d| {
                        // Cast: the fake array's address.
                        mov_imm64_full(b, RDX, arg as i64);
                        emit_ir_string_from_chars(b, &s, d, false);
                    },
                    rbp,
                    0,
                )
            });
            assert_eq!((r, moved), (-1, 0), "{what}");
        }
    }

    /// `new String(char[])`: the receiver (R9) gets the fresh `byte[]` and
    /// its coder; a constructed, foreign, legacy or null receiver declines.
    #[test]
    fn string_init_from_chars_fills_the_receiver() {
        let guard: u32 = 5;
        let flag: u8 = 1;
        let s = site(&flag, &guard, 5);
        let data = cratonvm_types::ARRAY_DATA_OFFSET;
        let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
        // Cast (both): small offsets.
        let vo = VALUE_OFF as usize;
        let co = CODER.0 as usize;
        let receiver = |cid: u32, compact: bool, value: u64| -> Vec<u64> {
            let mut r = vec![0u8; 64];
            r[0..4].copy_from_slice(&cid.to_le_bytes());
            if compact {
                r[cratonvm_types::GC_FLAGS_BYTE_OFFSET] = cratonvm_types::GC_FLAG_COMPACT;
            }
            r[vo..vo + 8].copy_from_slice(&value.to_le_bytes());
            r[co] = 0xAB;
            words(&r)
        };
        let latin = array(
            "[C",
            9,
            &u16le(&[0x61, 0x62, 0xE9, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69]),
        );
        let wide = array("[C", 3, &u16le(&[0x61, 0x3A9, 0x62]));
        let cases: [(&Vec<u64>, usize, u8); 2] = [(&latin, 9, 0), (&wide, 6, 1)];
        for sse in SSE_ARMS {
            for (chars, blen, coder) in cases {
                let mut recv = receiver(STRING_CID, true, 0);
                // Cast: the receiver's address.
                let recv_addr = recv.as_mut_ptr() as u64;
                let (r, t, base, moved) = with_tlab(64, |rbp| {
                    cratonvm_types::flags::with_thread_overrides(
                        &[("CRATONVM_JIT_IR_STRING_SSE2", sse)],
                        || {
                            run(
                                |b, d| {
                                    // Cast: the fake array's address.
                                    mov_imm64_full(b, RDX, chars.as_ptr() as i64);
                                    emit_ir_string_from_chars(b, &s, d, true);
                                },
                                rbp,
                                recv_addr,
                            )
                        },
                    )
                });
                assert_ne!(r, -1, "sse={sse:?} coder={coder}: not declined");
                let rr = raw(&recv);
                let value = u64::from_le_bytes(rr[vo..vo + 8].try_into().expect("8 bytes"));
                assert_eq!(value, base, "value = the fresh byte[]");
                assert_eq!(rr[co], coder, "coder written");
                assert_eq!(
                    moved as usize,
                    data + blen.div_ceil(8) * 8,
                    "the byte[] alone"
                ); // Cast: small
                assert_eq!(header_u32(&t, len_at) as usize, blen);
            }
        }
        let declines: [(u32, bool, u64, &str); 3] = [
            (STRING_CID, true, 0x1000, "already constructed"),
            (STRING_CID + 1, true, 0, "foreign class"),
            (STRING_CID, false, 0, "legacy layout"),
        ];
        for (cid, compact, value, what) in declines {
            let mut recv = receiver(cid, compact, value);
            // Cast: the receiver's address.
            let recv_addr = recv.as_mut_ptr() as u64;
            let (r, _, _, moved) = with_tlab(64, |rbp| {
                run(
                    |b, d| {
                        // Cast: the fake array's address.
                        mov_imm64_full(b, RDX, latin.as_ptr() as i64);
                        emit_ir_string_from_chars(b, &s, d, true);
                    },
                    rbp,
                    recv_addr,
                )
            });
            assert_eq!((r, moved), (-1, 0), "{what}");
            assert_eq!(raw(&recv)[co], 0xAB, "{what}: nothing stored");
        }
        let (r, _, _, moved) = with_tlab(64, |rbp| {
            run(
                |b, d| {
                    // Cast: the fake array's address.
                    mov_imm64_full(b, RDX, latin.as_ptr() as i64);
                    emit_ir_string_from_chars(b, &s, d, true);
                },
                rbp,
                0,
            )
        });
        assert_eq!((r, moved), (-1, 0), "null receiver");
    }

    /// The Generational receiver screen: inside either young semispace
    /// passes, anything else (below, between, at an end) declines.
    #[test]
    fn the_payload_pass_judges_what_it_read() {
        // rev12: the checked compress takes its wide edge on a unit above 0xFF
        // (in the SSE step and in the scalar tail), and the private-copy scan
        // reports "no wide unit" exactly for an all-LATIN1 UTF16 payload.
        let compress = |units: &[u16]| -> (i64, Vec<u8>) {
            let src = units.to_vec();
            let mut dst = vec![0xAAu8; units.len() + 8];
            let (s, d) = (src.as_ptr() as i64, dst.as_mut_ptr() as i64); // Cast: addresses
            let r = run(
                |b, dl| {
                    mov_imm64_full(b, RDX, s);
                    mov_imm64_full(b, R8, d);
                    // Cast: a small count.
                    mov_imm32_sx(b, RCX, units.len() as i32);
                    dl.extend(emit_chars_compress_checked(b));
                    mov_imm32_sx(b, RAX, 1);
                },
                0,
                0,
            );
            (r, dst)
        };
        for sse in SSE_ARMS {
            let latin: Vec<u16> = (0..21u16).map(|i| 0x41 + i * 9).collect();
            let (r, d) = cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_JIT_IR_STRING_SSE2", sse)],
                || compress(&latin),
            );
            assert_eq!(r, 1, "sse={sse:?}: all LATIN1 compresses");
            let want: Vec<u8> = latin.iter().map(|&u| u.to_le_bytes()[0]).collect();
            assert_eq!(&d[..21], &want[..]);
            for at in [0usize, 7, 15, 20] {
                let mut w = latin.clone();
                w[at] = 0x100;
                let (r, _) = cratonvm_types::flags::with_thread_overrides(
                    &[("CRATONVM_JIT_IR_STRING_SSE2", sse)],
                    || compress(&w),
                );
                assert_eq!(
                    r, -1,
                    "sse={sse:?}: a wide unit at {at} rejects the payload"
                );
            }
        }
        let scan = |units: &[u16]| -> i64 {
            // Cast: a small count.
            let arr = array("[B", 2 * units.len() as u32, &u16le(units));
            let a = arr.as_ptr() as i64; // Cast: the fake array's address
            let len_d8 = u8::try_from(cratonvm_types::ARRAY_LENGTH_OFFSET).expect("disp8");
            let data = i32::try_from(cratonvm_types::ARRAY_DATA_OFFSET).expect("disp32");
            run(
                |b, dl| {
                    mov_imm64_full(b, R11, a);
                    dl.push(emit_private_utf16_has_no_wide_unit(b, len_d8, data));
                    mov_imm32_sx(b, RAX, 1);
                },
                0,
                0,
            )
        };
        let mut u: Vec<u16> = vec![0xE9; 13];
        assert_eq!(scan(&u), -1, "all LATIN1 in a UTF16 copy: abandon");
        for at in [0usize, 3, 4, 12] {
            u = vec![0xE9; 13];
            u[at] = 0x3A9;
            assert_eq!(scan(&u), 1, "a wide unit at {at}: keep UTF16");
        }
    }

    #[test]
    fn the_young_receiver_check_reads_both_semispaces() {
        let table: [u64; 6] = [0x1000, 0x2000, 0x8000, 0x9000, 0x20000, 0x30000];
        // Cast: the fake table's address.
        let t = table.as_ptr() as usize;
        let check = |addr: u64| {
            run(
                |b, d| {
                    emit_young_receiver_check(b, t, d);
                    mov_imm32_sx(b, RAX, 1);
                },
                0,
                addr,
            )
        };
        let cases: [(u64, i64); 9] = [
            (0x1000, 1),
            (0x1FF8, 1),
            (0x8000, 1),
            (0x8FF8, 1),
            (0x0FF8, -1),
            (0x2000, -1),
            (0x5000, -1),
            (0x9000, -1),
            (0x20010, -1),
        ];
        for (addr, want) in cases {
            assert_eq!(check(addr), want, "{addr:#x}");
        }
    }
}

/// JIT r20 wave 18, lane xSb (STR20-2): the two builder emitters
/// ([`emit_sb_append_int_latin1`], [`emit_ir_sb_to_string`]) EXECUTED over
/// fake arrays, a fake thread and a fake TLAB: the digit text (every sign and
/// width, `Integer.MIN_VALUE` / `MAX_VALUE`) and the new count, the fresh
/// LATIN1 String, and every decline leaving the array / cursor untouched.
#[cfg(all(test, target_arch = "x86_64"))]
mod r20_xsb_builder_tests {
    use super::*;

    #[cfg(windows)]
    const A0: u8 = 1; // RCX
    #[cfg(windows)]
    const A1: u8 = 2; // RDX
    #[cfg(not(windows))]
    const A0: u8 = 7; // RDI
    #[cfg(not(windows))]
    const A1: u8 = 6; // RSI

    const STRING_CID: u32 = 77;
    const TOTAL: i32 = 32;
    const VALUE_OFF: i32 = 16;
    const CODER: (i32, bool) = (24, true);

    fn words(raw: &[u8]) -> Vec<u64> {
        let mut w = vec![0u64; raw.len().div_ceil(8)];
        for (i, b) in raw.iter().enumerate() {
            w[i / 8] |= u64::from(*b) << ((i % 8) * 8);
        }
        w
    }

    fn raw(w: &[u64]) -> Vec<u8> {
        w.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    /// A fake array of kind `desc` and header length `len`: `payload`, then
    /// `0xEE` filler up to `len` bytes (and 8 more).
    fn array(desc: &str, len: u32, payload: &[u8]) -> Vec<u64> {
        let data = cratonvm_types::ARRAY_DATA_OFFSET;
        let n = (len as usize).max(payload.len()); // Cast: a small count
        let mut r = vec![0xEEu8; data + n + 8];
        r[..data].fill(0);
        r[cratonvm_types::KIND_TAGS_BYTE_OFFSET] =
            cratonvm_types::primitive_array_kind_tags_byte(desc).expect("array tag");
        let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
        r[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
        r[data..data + payload.len()].copy_from_slice(payload);
        words(&r)
    }

    /// Run `append(int)` over `value` with `count` and `i` (RCX = value,
    /// R11D = count, R10D = i), answering the new count (R9D) or -1 on a
    /// decline.
    fn append_int(value: &mut [u64], count: u32, i: i32) -> i64 {
        let mut buf = ExecutableBuffer::new(1 << 16).expect("test buffer");
        mov_r64_r64(&mut buf, RAX, A1); // RAX = count << 32 | (u32) i
        mov_r64_r64(&mut buf, RCX, A0);
        mov_r64_r64(&mut buf, R11, RAX);
        shr_r64_imm8(&mut buf, R11, 32);
        buf.emit(&[0x41, 0x89, 0xC2]); // MOV R10D,EAX
        let mut decline = Vec::new();
        emit_sb_append_int_latin1(&mut buf, &mut decline);
        buf.emit(&[0x4C, 0x89, 0xC8]); // MOV RAX,R9
        buf.emit_byte(0xC3); // RET
        for p in decline {
            patch_here(&mut buf, p);
        }
        mov_imm32_sx(&mut buf, RAX, -1);
        buf.emit_byte(0xC3); // RET
        assert!(!buf.overflowed());
        crate::platform::make_executable(buf.as_ptr() as *mut u8, buf.capacity())
            .expect("the test buffer must be flippable to RX");
        // SAFETY: a leaf snippet over the caller's buffer (alive across the
        // call) and caller-saved registers only.
        let f: extern "C" fn(u64, u64) -> i64 = unsafe { std::mem::transmute(buf.as_ptr()) };
        // Cast: the fake array's address; the packed operands.
        f(
            value.as_mut_ptr() as u64,
            (u64::from(count) << 32) | u64::from(i as u32),
        )
    }

    #[test]
    fn append_int_writes_integer_to_string_in_place() {
        let data = cratonvm_types::ARRAY_DATA_OFFSET;
        let ints: [i32; 12] = [
            0,
            7,
            -7,
            42,
            -10,
            99,
            1_000_000_000,
            -1_000_000_000,
            999_999_999,
            i32::MAX,
            i32::MIN,
            -2_147_483_647,
        ];
        for i in ints {
            let text = i.to_string();
            let prefix = b"ab=";
            // Cast (both): small counts.
            let count = prefix.len() as u32;
            let exact = count + text.len() as u32;
            // Room to spare, and room exactly.
            for cap in [exact + 5, exact] {
                let mut v = array("[B", cap, prefix);
                let r = append_int(&mut v, count, i);
                assert_eq!(r, i64::from(exact), "append({i}) cap {cap}: the new count");
                let bytes = raw(&v);
                let at = data + prefix.len();
                assert_eq!(&bytes[data..at], prefix, "the old content is kept");
                assert_eq!(&bytes[at..at + text.len()], text.as_bytes(), "append({i})");
                let end = data + cap as usize; // Cast: a small count
                assert!(
                    bytes[at + text.len()..end].iter().all(|&b| b == 0xEE),
                    "append({i}): nothing written past the digits"
                );
            }
            // One byte short: the growth is the call's, nothing stored.
            let mut v = array("[B", exact - 1, prefix);
            let before = raw(&v);
            assert_eq!(append_int(&mut v, count, i), -1, "append({i}) needs growth");
            assert_eq!(
                raw(&v),
                before,
                "append({i}): the declined array is untouched"
            );
        }
        // A count past the array (corrupt) declines too.
        let mut v = array("[B", 4, b"abcd");
        assert_eq!(append_int(&mut v, 9, 1), -1);
    }

    fn site(flag: &u8, guard: &u32, guard_value: u32) -> CharsIrSite {
        CharsIrSite {
            alloc: SbToStringPlan::for_test(
                STRING_CID,
                TOTAL,
                VALUE_OFF,
                // Cast: the guard word's address.
                (guard as *const u32 as usize, guard_value),
            ),
            gc: SbToStringGc {
                zgc_start_bits: false,
                g1_same_region: false,
            },
            thread: ConcatThread::Frame(8),
            cursor_off: 8,
            end_off: 16,
            g1_table: 0,
            class_id_off: 0,
            // Cast: the flag's address.
            compact_flag_addr: flag as *const u8 as usize,
            young_bounds_addr: 0,
            coder: CODER,
            kind_d8: u8::try_from(cratonvm_types::KIND_TAGS_BYTE_OFFSET).expect("disp8"),
            byte_array_tag: cratonvm_types::primitive_array_kind_tags_byte("[B").expect("[B"),
            char_array_tag: cratonvm_types::primitive_array_kind_tags_byte("[C").expect("[C"),
            char_array_mark: 0,
        }
    }

    /// `toString()` over a TLAB of `tlab_words` with RDX = `value` and ECX =
    /// `count`: answers the tail's RAX (-1 on a decline), the TLAB's bytes,
    /// its base and how far the cursor moved.
    fn to_string(
        s: &CharsIrSite,
        value: u64,
        count: u32,
        tlab_words: usize,
    ) -> (i64, Vec<u8>, u64, u64) {
        let mut tlab = vec![0u64; tlab_words];
        // Cast: addresses as words.
        let base = tlab.as_mut_ptr() as u64;
        let mut thread = vec![0u64; 4];
        thread[1] = base;
        // Cast: a small count.
        thread[2] = base + 8 * tlab_words as u64;
        // Cast: the thread's address as the frame word.
        let frame = [thread.as_ptr() as u64, 0];
        // SAFETY: one past `frame[0]`, inside the array.
        let rbp = unsafe { frame.as_ptr().add(1) } as u64;
        let mut buf = ExecutableBuffer::new(1 << 16).expect("test buffer");
        buf.emit_byte(0x55); // PUSH RBP
        mov_r64_r64(&mut buf, 5, A0); // MOV RBP, a0
        let mut decline = Vec::new();
        // Cast: the fake array's address.
        mov_imm64_full(&mut buf, RDX, value as i64);
        buf.emit_byte(0xB9); // MOV ECX, imm32 (zero-extends)
        buf.emit(&count.to_le_bytes());
        emit_ir_sb_to_string(&mut buf, s, &mut decline);
        buf.emit(&[0x5D, 0xC3]); // POP RBP ; RET
        for p in decline {
            patch_here(&mut buf, p);
        }
        mov_imm32_sx(&mut buf, RAX, -1);
        buf.emit(&[0x5D, 0xC3]); // POP RBP ; RET
        assert!(!buf.overflowed());
        crate::platform::make_executable(buf.as_ptr() as *mut u8, buf.capacity())
            .expect("the test buffer must be flippable to RX");
        // SAFETY: a leaf snippet over the caller's buffers (alive across the
        // call) and caller-saved registers; RBP is saved and restored.
        let f: extern "C" fn(u64, u64) -> i64 = unsafe { std::mem::transmute(buf.as_ptr()) };
        let r = f(rbp, 0);
        let moved = thread[1] - base;
        (r, raw(&tlab), base, moved)
    }

    fn header_u32(raw: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(raw[at..at + 4].try_into().expect("4 bytes"))
    }

    #[test]
    fn to_string_builds_a_fresh_latin1_string_of_count_bytes() {
        let flag: u8 = 1;
        let guard: u32 = 5;
        let s = site(&flag, &guard, 5);
        let data = cratonvm_types::ARRAY_DATA_OFFSET;
        let len_at = cratonvm_types::ARRAY_LENGTH_OFFSET;
        let text: &[u8] = b"GET /index.html HTTP/1.1 h\xe9llo -2147483648";
        // Capacity past the count: only `count` bytes are copied.
        for count in [1usize, 7, 8, 9, text.len()] {
            // Cast (both): small counts.
            let value = array("[B", text.len() as u32 + 6, text);
            let (r, t, base, moved) = to_string(&s, value.as_ptr() as u64, count as u32, 64);
            assert_ne!(r, -1, "count {count}: not declined");
            assert_eq!(header_u32(&t, len_at) as usize, count, "a byte[count]");
            assert_eq!(
                &t[data..data + count],
                &text[..count],
                "count {count}: the payload"
            );
            let string_at = data + count.div_ceil(8) * 8;
            // Cast: the answer back to an offset into the TLAB.
            assert_eq!(r as u64 - base, string_at as u64, "RAX = the String");
            assert_eq!(header_u32(&t, string_at), STRING_CID);
            let vo = string_at + VALUE_OFF as usize; // Cast: a small offset
            assert_eq!(
                u64::from_le_bytes(t[vo..vo + 8].try_into().expect("8 bytes")),
                base,
                "String.value is the fresh byte[]"
            );
            let co = string_at + CODER.0 as usize; // Cast: a small offset
            assert_eq!(t[co], 0, "coder LATIN1");
            // Cast: small offsets.
            assert_eq!(
                moved as usize,
                string_at + TOTAL as usize,
                "one bump, committed"
            );
        }
        // Declines, each with the cursor untouched: an empty builder, a count
        // past the array, a null value, a char[] value, a replaced String
        // layout, compact strings off, a full TLAB.
        let value = array("[B", 4, b"abcd");
        let chars = array("[C", 4, b"a\0b\0c\0d\0");
        let off: u8 = 0;
        let replaced = site(&flag, &guard, 6);
        let compact_off = site(&off, &guard, 5);
        // Cast: the fake arrays' addresses.
        let (v, c) = (value.as_ptr() as u64, chars.as_ptr() as u64);
        let declines: [(&CharsIrSite, u64, u32, usize, &str); 7] = [
            (&s, v, 0, 64, "empty builder"),
            (&s, v, 5, 64, "count past the array"),
            (&s, 0, 2, 64, "null value"),
            (&s, c, 2, 64, "char[] value"),
            (&replaced, v, 2, 64, "replaced layout"),
            (&compact_off, v, 2, 64, "compact strings off"),
            (&s, v, 4, 4, "full TLAB"),
        ];
        for (site, value, count, tlab_words, what) in declines {
            let (r, _, _, moved) = to_string(site, value, count, tlab_words);
            assert_eq!((r, moved), (-1, 0), "{what}");
        }
    }

    /// JIT r21 wave 3, lane strings (ZCP-3): [`emit_ir_latin1_to_lower_string`]
    /// over the same fake thread / TLAB: a fresh LATIN1 String whose bytes are
    /// `CharacterDataLatin1.toLowerCase` of each source byte -- checked for
    /// EVERY Latin-1 byte against Rust's `char::to_lowercase` (single-char
    /// results; all 256 are) -- and the same declines as `toString()`.
    #[test]
    fn to_lower_string_maps_every_latin1_byte() {
        let flag: u8 = 1;
        let guard: u32 = 5;
        let s = site(&flag, &guard, 5);
        let data = cratonvm_types::ARRAY_DATA_OFFSET;
        let all: Vec<u8> = (0..=255u8).collect();
        let want: Vec<u8> = all
            .iter()
            .map(|&b| {
                let mut it = char::from(b).to_lowercase();
                match (it.next(), it.next()) {
                    // Cast: a Latin-1 lower case is Latin-1.
                    (Some(c), None) if (c as u32) < 0x100 => c as u8,
                    _ => b,
                }
            })
            .collect();
        for (from, to) in [(0usize, 256usize), (0x40, 0x5C), (0xBE, 0xE1), (0, 1), (0x41, 0x42)] {
            let text = &all[from..to];
            // Cast (both): small counts.
            let value = array("[B", text.len() as u32, text);
            let (r, t, base, _) = to_lower(&s, value.as_ptr() as u64, text.len() as u32, 128);
            assert_ne!(r, -1, "{from:#x}..{to:#x}: not declined");
            assert_eq!(&t[data..data + text.len()], &want[from..to], "{from:#x}..{to:#x}");
            let string_at = data + text.len().div_ceil(8) * 8;
            // Cast: the answer back to an offset into the TLAB.
            assert_eq!(r as u64 - base, string_at as u64, "RAX = the String");
            let co = string_at + CODER.0 as usize; // Cast: a small offset
            assert_eq!(t[co], 0, "coder LATIN1");
        }
        let value = array("[B", 4, b"ABCD");
        let off: u8 = 0;
        let compact_off = site(&off, &guard, 5);
        // Cast: the fake array's address.
        let v = value.as_ptr() as u64;
        for (site, count, tlab_words, what) in [
            (&s, 4u32, 4usize, "full TLAB"),
            (&compact_off, 4, 64, "compact strings off"),
            (&s, 5, 64, "count past the array"),
        ] {
            let (r, _, _, moved) = to_lower(site, v, count, tlab_words);
            assert_eq!((r, moved), (-1, 0), "{what}");
        }
    }

    /// [`to_string`] with the lower-case emitter.
    fn to_lower(
        s: &CharsIrSite,
        value: u64,
        count: u32,
        tlab_words: usize,
    ) -> (i64, Vec<u8>, u64, u64) {
        let mut tlab = vec![0u64; tlab_words];
        // Cast: addresses as words.
        let base = tlab.as_mut_ptr() as u64;
        let mut thread = vec![0u64; 4];
        thread[1] = base;
        // Cast: a small count.
        thread[2] = base + 8 * tlab_words as u64;
        // Cast: the thread's address as the frame word.
        let frame = [thread.as_ptr() as u64, 0];
        // SAFETY: one past `frame[0]`, inside the array.
        let rbp = unsafe { frame.as_ptr().add(1) } as u64;
        let mut buf = ExecutableBuffer::new(1 << 16).expect("test buffer");
        buf.emit_byte(0x55); // PUSH RBP
        mov_r64_r64(&mut buf, 5, A0); // MOV RBP, a0
        let mut decline = Vec::new();
        // Cast: the fake array's address.
        mov_imm64_full(&mut buf, RDX, value as i64);
        buf.emit_byte(0xB9); // MOV ECX, imm32 (zero-extends)
        buf.emit(&count.to_le_bytes());
        emit_ir_latin1_to_lower_string(&mut buf, s, &mut decline);
        buf.emit(&[0x5D, 0xC3]); // POP RBP ; RET
        for p in decline {
            patch_here(&mut buf, p);
        }
        mov_imm32_sx(&mut buf, RAX, -1);
        buf.emit(&[0x5D, 0xC3]); // POP RBP ; RET
        assert!(!buf.overflowed());
        crate::platform::make_executable(buf.as_ptr() as *mut u8, buf.capacity())
            .expect("the test buffer must be flippable to RX");
        // SAFETY: a leaf snippet over the caller's buffers (alive across the
        // call) and caller-saved registers; RBP is saved and restored.
        let f: extern "C" fn(u64, u64) -> i64 = unsafe { std::mem::transmute(buf.as_ptr()) };
        let r = f(rbp, 0);
        let moved = thread[1] - base;
        (r, raw(&tlab), base, moved)
    }
}
