// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! All-primitive inline `System.arraycopy` fast prefix for IR lowering.
//!
//! Covers all eight primitive array types (`[Z`, `[B`, `[C`, `[S`, `[I`,
//! `[F`, `[J`, `[D`) -- including self-array overlapping copies (`src == dst`)
//! -- across Serial/Generational, G1, and ZGC. Any null operand, non-array,
//! reference array, mismatched primitive component type, negative position/len,
//! or out-of-bounds range branches to the `decline` fall-through (reference
//! arm or `jit_system_arraycopy_direct`) without writing a single byte.

use super::{Lowerer, NodeId, R10, R11, R8, R9, RAX, RCX, RDX};
use crate::ir::{Graph, IrType, Op};
use crate::runtime_lowering::patch_rel32_to_here;
use crate::ExecutableBuffer;

const KIND_TAGS: u8 =
    crate::x64::disp::disp8_const(cratonvm_types::KIND_TAGS_BYTE_OFFSET as i64) as u8;
const LENGTH: u8 = crate::x64::disp::disp8_const(cratonvm_types::ARRAY_LENGTH_OFFSET as i64) as u8;
const DATA: u8 = crate::x64::disp::disp8_const(cratonvm_types::ARRAY_DATA_OFFSET as i64) as u8;

/// Base of the contiguous 8-entry primitive-array `KIND_TAGS_BYTE` range:
/// `ObjectKind::Array | (ArrayElementType::Boolean << 2) == 0x11`.
const PRIM_ARRAY_TAG_BASE: u8 = (cratonvm_types::ObjectKind::Array as u8)
    | ((cratonvm_types::ArrayElementType::Boolean as u8) << 2);

/// Mask of bits that MUST be zero in `(tag.wrapping_sub(PRIM_ARRAY_TAG_BASE))`
/// for `tag` to be one of the 8 primitive-array `KIND_TAGS_BYTE` values
/// (`0x11, 0x15, 0x19, 0x1D, 0x21, 0x25, 0x29, 0x2D`).
const PRIM_ARRAY_TAG_INVALID_MASK: u8 = 0xE3;

const fn verify_prim_tag_formula() -> bool {
    use cratonvm_types::{ArrayElementType, ObjectKind};
    let expected: [(ArrayElementType, u8); 8] = [
        (ArrayElementType::Boolean, 0),
        (ArrayElementType::Char, 1),
        (ArrayElementType::Float, 2),
        (ArrayElementType::Double, 3),
        (ArrayElementType::Byte, 0),
        (ArrayElementType::Short, 1),
        (ArrayElementType::Int, 2),
        (ArrayElementType::Long, 3),
    ];
    let mut b: u16 = 0;
    while b < 256 {
        let tag = b as u8;
        let ok = (tag.wrapping_sub(PRIM_ARRAY_TAG_BASE) & PRIM_ARRAY_TAG_INVALID_MASK) == 0;
        let mut found_shift: Option<u8> = None;
        let mut i = 0;
        while i < expected.len() {
            let prim_tag = (ObjectKind::Array as u8) | ((expected[i].0 as u8) << 2);
            if tag == prim_tag {
                found_shift = Some(expected[i].1);
            }
            i += 1;
        }
        match found_shift {
            Some(s) => {
                if !ok || (((tag.wrapping_sub(PRIM_ARRAY_TAG_BASE)) >> 2) & 3) != s {
                    return false;
                }
            }
            None => {
                if ok {
                    return false;
                }
            }
        }
        b += 1;
    }
    true
}

const _: () = assert!(
    verify_prim_tag_formula(),
    "primitive-array KIND_TAGS formula must admit exactly the 8 primitive array types and compute log2(elem_size)"
);

#[inline]
fn const_i32(graph: &Graph, id: NodeId) -> Option<i32> {
    match graph.nodes.get(id as usize).map(|n| (&n.op, n.ty)) {
        Some((Op::Const(c), IrType::Int)) => Some(*c as i32),
        _ => None,
    }
}

#[inline]
fn jcc32(buf: &mut ExecutableBuffer, cc: u8) -> usize {
    buf.emit(&[0x0F, cc]);
    let patch = buf.pos();
    buf.emit(&[0; 4]);
    patch
}

#[inline]
fn jmp32(buf: &mut ExecutableBuffer) -> usize {
    buf.emit_byte(0xE9);
    let patch = buf.pos();
    buf.emit(&[0; 4]);
    patch
}

#[inline]
fn prim_array_tag_from_atype(atype: u8) -> Option<u8> {
    use cratonvm_types::{ArrayElementType, ObjectKind};
    let elem = match atype {
        4 => ArrayElementType::Boolean,
        5 => ArrayElementType::Char,
        6 => ArrayElementType::Float,
        7 => ArrayElementType::Double,
        8 => ArrayElementType::Byte,
        9 => ArrayElementType::Short,
        10 => ArrayElementType::Int,
        11 => ArrayElementType::Long,
        _ => return None,
    };
    Some((ObjectKind::Array as u8) | ((elem as u8) << 2))
}

#[inline]
fn known_prim_array_facts(lo: &Lowerer<'_>, id: NodeId) -> (bool, Option<u8>, Option<i32>) {
    if let Some(node) = lo.graph.nodes.get(id as usize) {
        match node.op {
            Op::IndyRootRef { slot_addr } => {
                if let Some(m) = lo.graph.const_ref_meta.get(&slot_addr) {
                    return (m.non_null, m.prim_array_tag, m.array_length);
                }
            }
            Op::NewArray { element_type, .. } if element_type != 0 => {
                let len = node
                    .inputs
                    .get(2)
                    .and_then(|&ln| const_i32(lo.graph, ln))
                    .filter(|&v| v >= 0);
                return (true, prim_array_tag_from_atype(element_type), len);
            }
            _ => {}
        }
    }
    (lo.receiver_already_non_null(id), None, None)
}

#[inline]
fn emit_prim_len_guard(
    lo: &mut Lowerer<'_>,
    reg_modrm_rm: u8,
    min_len: i32,
    decline: &mut Vec<usize>,
) {
    // CMP DWORD [RAX/RDX + LENGTH], min_len ; JL decline
    lo.buf.emit(&[0x81, 0x78 | reg_modrm_rm, LENGTH]);
    lo.buf.emit(&(min_len as u32).to_le_bytes());
    decline.push(lo.emit_jcc_rel32(0x8C));
}

#[inline]
fn emit_fixed_32_or_64_copy(lo: &mut Lowerer<'_>, bytes: usize) {
    // Load all bytes from [RAX + DATA ..] before any store to [RDX + DATA ..] for overlap safety.
    lo.buf.emit(&[0x0F, 0x10, 0x40, DATA]); // MOVUPS XMM0, [RAX + DATA]
    lo.buf.emit(&[0x0F, 0x10, 0x48, DATA + 16]); // MOVUPS XMM1, [RAX + DATA + 16]
    if bytes == 64 {
        lo.buf.emit(&[0x4C, 0x8B, 0x40, DATA + 32]); // MOV R8, [RAX + DATA + 32]
        lo.buf.emit(&[0x4C, 0x8B, 0x48, DATA + 40]); // MOV R9, [RAX + DATA + 40]
        lo.buf.emit(&[0x4C, 0x8B, 0x50, DATA + 48]); // MOV R10, [RAX + DATA + 48]
        lo.buf.emit(&[0x4C, 0x8B, 0x58, DATA + 56]); // MOV R11, [RAX + DATA + 56]
    }
    lo.buf.emit(&[0x0F, 0x11, 0x42, DATA]); // MOVUPS [RDX + DATA], XMM0
    lo.buf.emit(&[0x0F, 0x11, 0x4A, DATA + 16]); // MOVUPS [RDX + DATA + 16], XMM1
    if bytes == 64 {
        lo.buf.emit(&[0x4C, 0x89, 0x42, DATA + 32]); // MOV [RDX + DATA + 32], R8
        lo.buf.emit(&[0x4C, 0x89, 0x4A, DATA + 40]); // MOV [RDX + DATA + 40], R9
        lo.buf.emit(&[0x4C, 0x89, 0x52, DATA + 48]); // MOV [RDX + DATA + 48], R10
        lo.buf.emit(&[0x4C, 0x89, 0x5A, DATA + 56]); // MOV [RDX + DATA + 56], R11
    }
}

/// Emit the all-primitive `System.arraycopy` inline prefix arm.
///
/// Returns `Some(hit)` where `hit` is a rel32 `JMP` to patch to the call's
/// end, or `None` if a compile-time constant argument makes the call
/// unconditionally decline (negative `srcPos`, `dstPos`, or `len`).
///
/// Scratch: `RAX`, `RCX`, `RDX`, `R8`..`R11`, `XMM0`, `XMM1`, and flags.
/// Caller (`try_emit_ir_ref_arraycopy_prefix`) must already have rejected live
/// carries and called `note_rcx_written()`.
pub(in crate::ir_lower) fn emit_ir_prim_arraycopy_arm(
    lo: &mut Lowerer<'_>,
    (src, src_pos, dst, dst_pos, len): (NodeId, NodeId, NodeId, NodeId, NodeId),
) -> Option<usize> {
    let const_len = const_i32(lo.graph, len);
    let const_src_pos = const_i32(lo.graph, src_pos);
    let const_dst_pos = const_i32(lo.graph, dst_pos);
    if const_len.is_some_and(|v| v < 0)
        || const_src_pos.is_some_and(|v| v < 0)
        || const_dst_pos.is_some_and(|v| v < 0)
    {
        return None;
    }

    let (src_nn, src_tag, src_len) = known_prim_array_facts(lo, src);
    let (dst_nn, dst_tag, dst_len) = known_prim_array_facts(lo, dst);
    let known_same_shift = src_tag
        .zip(dst_tag)
        .and_then(|(s, d)| (s == d).then_some(((s.wrapping_sub(PRIM_ARRAY_TAG_BASE)) >> 2) & 3));

    let mut decline: Vec<usize> = Vec::new();
    let mut hit_jumps: Vec<usize> = Vec::new();

    // --- 1. Null checks (`src != null`, `dst != null`). ---
    lo.gp_load_value(RAX, src);
    lo.gp_load_value(RDX, dst);
    if !src_nn {
        lo.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        decline.push(lo.emit_jcc_rel32(0x84)); // JZ
    }
    if !dst_nn {
        lo.buf.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
        decline.push(lo.emit_jcc_rel32(0x84)); // JZ
    }

    // Specialized fixed 32B / 64B overlap-safe copy when types, positions (=0), and len are known.
    if let (Some(shift), Some(0), Some(0), Some(c)) =
        (known_same_shift, const_src_pos, const_dst_pos, const_len)
    {
        if c > 0 {
            if let Some(bytes) = usize::try_from(c)
                .ok()
                .and_then(|n| n.checked_shl(u32::from(shift)))
            {
                if bytes == 32 || bytes == 64 {
                    if !src_len.is_some_and(|l| l >= c) {
                        emit_prim_len_guard(lo, 0, c, &mut decline);
                    }
                    if !dst_len.is_some_and(|l| l >= c) {
                        emit_prim_len_guard(lo, 2, c, &mut decline);
                    }
                    emit_fixed_32_or_64_copy(lo, bytes);
                    crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
                    let hit = lo.emit_jmp_rel32();
                    for p in decline {
                        lo.patch_rel32_to_here(p);
                    }
                    return Some(hit);
                }
            }
        }
    }

    // --- 2. Header tag validation & log2 element shift -> CL (0..=3). ---
    if let Some(shift) = known_same_shift {
        if const_len != Some(0) {
            lo.buf.emit(&[0xB1, shift]); // MOV CL, elem_shift
        }
    } else {
        lo.buf.emit(&[0x0F, 0xB6, 0x48, KIND_TAGS]); // MOVZX ECX, BYTE [RAX + KIND_TAGS]
        lo.buf.emit(&[0x3A, 0x4A, KIND_TAGS]); // CMP CL, BYTE [RDX + KIND_TAGS]
        decline.push(lo.emit_jcc_rel32(0x85)); // JNE: mismatched array/element types
        lo.buf.emit(&[0x80, 0xE9, PRIM_ARRAY_TAG_BASE]); // SUB CL, 0x11
        lo.buf.emit(&[0xF6, 0xC1, PRIM_ARRAY_TAG_INVALID_MASK]); // TEST CL, 0xE3
        decline.push(lo.emit_jcc_rel32(0x85)); // JNZ: not one of the 8 primitive arrays
        lo.buf.emit(&[0xC0, 0xE9, 0x02]); // SHR CL, 2
        lo.buf.emit(&[0x80, 0xE1, 0x03]); // AND CL, 3
    }

    // --- 3. Load `len` -> R11 (>= 0) and check `src` / `dst` ranges. ---
    match const_len {
        Some(0) => {
            lo.buf.emit(&[0x45, 0x31, 0xDB]); // XOR R11D, R11D
        }
        Some(c) => {
            lo.buf.emit(&[0x41, 0xBB]); // MOV R11D, imm32 (zero-extends to R11)
            lo.buf.emit(&(c as u32).to_le_bytes());
        }
        None => {
            lo.gp_load_value(R11, len);
            lo.buf.emit(&[0x4D, 0x63, 0xDB]); // MOVSXD R11, R11D
            lo.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
            decline.push(lo.emit_jcc_rel32(0x88)); // JS
        }
    }

    // Source bounds: `0 <= srcPos` and `srcPos + len <= src.length` (no i32 wrap).
    match const_src_pos {
        Some(0) => {
            lo.buf.emit(&[0x44, 0x8B, 0x48, LENGTH]); // MOV R9D, [RAX + LENGTH]
            lo.buf.emit(&[0x4D, 0x39, 0xD9]); // CMP R9, R11
            decline.push(lo.emit_jcc_rel32(0x8C)); // JL
            if const_len != Some(0) {
                lo.buf.emit(&[0x4C, 0x8D, 0x50, DATA]); // LEA R10, [RAX + DATA]
            }
        }
        Some(p) => {
            lo.buf.emit(&[0x41, 0xBA]); // MOV R10D, imm32
            lo.buf.emit(&(p as u32).to_le_bytes());
            lo.buf.emit(&[0x44, 0x8B, 0x48, LENGTH]); // MOV R9D, [RAX + LENGTH]
            lo.buf.emit(&[0x4D, 0x29, 0xD1]); // SUB R9, R10
            lo.buf.emit(&[0x4D, 0x39, 0xD9]); // CMP R9, R11
            decline.push(lo.emit_jcc_rel32(0x8C)); // JL
            if const_len != Some(0) {
                lo.buf.emit(&[0x49, 0xD3, 0xE2]); // SHL R10, CL
                lo.buf.emit(&[0x4E, 0x8D, 0x54, 0x10, DATA]); // LEA R10, [RAX + R10 + DATA]
            }
        }
        None => {
            lo.gp_load_value(R10, src_pos);
            lo.buf.emit(&[0x4D, 0x63, 0xD2]); // MOVSXD R10, R10D
            lo.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
            decline.push(lo.emit_jcc_rel32(0x88)); // JS
            lo.buf.emit(&[0x44, 0x8B, 0x48, LENGTH]); // MOV R9D, [RAX + LENGTH]
            lo.buf.emit(&[0x4D, 0x29, 0xD1]); // SUB R9, R10
            lo.buf.emit(&[0x4D, 0x39, 0xD9]); // CMP R9, R11
            decline.push(lo.emit_jcc_rel32(0x8C)); // JL
            if const_len != Some(0) {
                lo.buf.emit(&[0x49, 0xD3, 0xE2]); // SHL R10, CL
                lo.buf.emit(&[0x4E, 0x8D, 0x54, 0x10, DATA]); // LEA R10, [RAX + R10 + DATA]
            }
        }
    }

    // Destination bounds: `0 <= dstPos` and `dstPos + len <= dst.length` (no i32 wrap).
    match const_dst_pos {
        Some(0) => {
            lo.buf.emit(&[0x44, 0x8B, 0x4A, LENGTH]); // MOV R9D, [RDX + LENGTH]
            lo.buf.emit(&[0x4D, 0x39, 0xD9]); // CMP R9, R11
            decline.push(lo.emit_jcc_rel32(0x8C)); // JL
        }
        Some(p) => {
            lo.buf.emit(&[0x41, 0xB8]); // MOV R8D, imm32
            lo.buf.emit(&(p as u32).to_le_bytes());
            lo.buf.emit(&[0x44, 0x8B, 0x4A, LENGTH]); // MOV R9D, [RDX + LENGTH]
            lo.buf.emit(&[0x4D, 0x29, 0xC1]); // SUB R9, R8
            lo.buf.emit(&[0x4D, 0x39, 0xD9]); // CMP R9, R11
            decline.push(lo.emit_jcc_rel32(0x8C)); // JL
            if const_len != Some(0) {
                lo.buf.emit(&[0x49, 0xD3, 0xE0]); // SHL R8, CL
            }
        }
        None => {
            lo.gp_load_value(R8, dst_pos);
            lo.buf.emit(&[0x4D, 0x63, 0xC0]); // MOVSXD R8, R8D
            lo.buf.emit(&[0x4D, 0x85, 0xC0]); // TEST R8, R8
            decline.push(lo.emit_jcc_rel32(0x88)); // JS
            lo.buf.emit(&[0x44, 0x8B, 0x4A, LENGTH]); // MOV R9D, [RDX + LENGTH]
            lo.buf.emit(&[0x4D, 0x29, 0xC1]); // SUB R9, R8
            lo.buf.emit(&[0x4D, 0x39, 0xD9]); // CMP R9, R11
            decline.push(lo.emit_jcc_rel32(0x8C)); // JL
            if const_len != Some(0) {
                lo.buf.emit(&[0x49, 0xD3, 0xE0]); // SHL R8, CL
            }
        }
    }

    if const_len == Some(0) {
        crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
        let hit = lo.emit_jmp_rel32();
        for p in decline {
            lo.patch_rel32_to_here(p);
        }
        return Some(hit);
    }

    if const_len.is_none() {
        lo.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
        hit_jumps.push(lo.emit_jcc_rel32(0x84)); // JZ: len == 0
    }

    // Scale `len` -> `byte_len` in R11, then finalize `dst_ptr` in RCX.
    lo.buf.emit(&[0x49, 0xD3, 0xE3]); // SHL R11, CL
    if const_dst_pos == Some(0) {
        lo.buf.emit(&[0x48, 0x8D, 0x4A, DATA]); // LEA RCX, [RDX + DATA]
    } else {
        lo.buf.emit(&[0x4A, 0x8D, 0x4C, 0x02, DATA]); // LEA RCX, [RDX + R8 + DATA]
    }

    // --- 4. Memmove-safe SIMD/GPR copy body (R10 = src, RCX = dst, R11 = byte_len > 0). ---
    let max_const_bytes = const_len.and_then(|c| {
        usize::try_from(c)
            .ok()?
            .checked_shl(u32::from(known_same_shift.unwrap_or(3)))
    });
    emit_prim_copy_body_nonzero(&mut lo.buf, max_const_bytes, &mut hit_jumps);

    for p in hit_jumps {
        lo.patch_rel32_to_here(p);
    }
    crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
    let hit = lo.emit_jmp_rel32();
    for p in decline {
        lo.patch_rel32_to_here(p);
    }
    Some(hit)
}

/// Emit the memmove-safe byte copy sequence when `byte_len` may be zero.
/// Entry: `R10 = src_ptr`, `RCX = dst_ptr`, `R11 = byte_len >= 0`.
/// Clobbers `RAX`, `RCX`, `RDX`, `R8`, `R9`, `R10`, `R11`, `XMM0`, `XMM1`, and flags.
#[cfg(test)]
pub(in crate::ir_lower) fn emit_prim_copy_bytes(buf: &mut ExecutableBuffer) {
    let mut hit_jumps = Vec::new();
    buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
    hit_jumps.push(jcc32(buf, 0x84)); // JZ done
    emit_prim_copy_body_nonzero(buf, None, &mut hit_jumps);
    for p in hit_jumps {
        patch_rel32_to_here(buf, p);
    }
}

/// Emit the memmove-safe byte copy body.
/// Entry: `R10 = src_ptr`, `RCX = dst_ptr`, `R11 = byte_len > 0`.
/// Every early exit pushes a rel32 jump onto `hit_jumps`; the final fall-through
/// exits straight past the emitted bytes.
fn emit_prim_copy_body_nonzero(
    buf: &mut ExecutableBuffer,
    max_const_bytes: Option<usize>,
    hit_jumps: &mut Vec<usize>,
) {
    let mut to_small: Vec<usize> = Vec::new();
    if !max_const_bytes.is_some_and(|b| b <= 32) {
        buf.emit(&[0x49, 0x83, 0xFB, 0x20]); // CMP R11, 32
        to_small.push(jcc32(buf, 0x86)); // JBE small_1_to_32

        // 33..=64 bytes: load all endpoints into {XMM0, XMM1, RAX, RDX, R8, R9} before storing.
        let gt64 = if max_const_bytes.is_some_and(|b| b <= 64) {
            None
        } else {
            buf.emit(&[0x49, 0x83, 0xFB, 0x40]); // CMP R11, 64
            Some(jcc32(buf, 0x87)) // JA gt64
        };
        buf.emit(&[0x41, 0x0F, 0x10, 0x02]); // MOVUPS XMM0, [R10]
        buf.emit(&[0x41, 0x0F, 0x10, 0x4A, 0x10]); // MOVUPS XMM1, [R10 + 16]
        buf.emit(&[0x4B, 0x8B, 0x44, 0x1A, 0xE0]); // MOV RAX, [R10 + R11 - 32]
        buf.emit(&[0x4B, 0x8B, 0x54, 0x1A, 0xE8]); // MOV RDX, [R10 + R11 - 24]
        buf.emit(&[0x4F, 0x8B, 0x44, 0x1A, 0xF0]); // MOV R8, [R10 + R11 - 16]
        buf.emit(&[0x4F, 0x8B, 0x4C, 0x1A, 0xF8]); // MOV R9, [R10 + R11 - 8]
        buf.emit(&[0x0F, 0x11, 0x01]); // MOVUPS [RCX], XMM0
        buf.emit(&[0x0F, 0x11, 0x49, 0x10]); // MOVUPS [RCX + 16], XMM1
        buf.emit(&[0x4A, 0x89, 0x44, 0x19, 0xE0]); // MOV [RCX + R11 - 32], RAX
        buf.emit(&[0x4A, 0x89, 0x54, 0x19, 0xE8]); // MOV [RCX + R11 - 24], RDX
        buf.emit(&[0x4E, 0x89, 0x44, 0x19, 0xF0]); // MOV [RCX + R11 - 16], R8
        buf.emit(&[0x4E, 0x89, 0x4C, 0x19, 0xF8]); // MOV [RCX + R11 - 8], R9
        hit_jumps.push(jmp32(buf));

        if let Some(gt) = gt64 {
            patch_rel32_to_here(buf, gt);
            // Backward only when `src_ptr < dst_ptr < src_ptr + byte_len`.
            buf.emit(&[0x4C, 0x39, 0xD1]); // CMP RCX, R10
            let fwd_low = jcc32(buf, 0x86); // JBE fwd
            buf.emit(&[0x4B, 0x8D, 0x04, 0x1A]); // LEA RAX, [R10 + R11]
            buf.emit(&[0x48, 0x39, 0xC1]); // CMP RCX, RAX
            let fwd_high = jcc32(buf, 0x83); // JAE fwd

            // Backward: 32-byte chunks from high addresses down, then small tail.
            buf.emit(&[0x49, 0x89, 0xC2]); // MOV R10, RAX (src_end)
            buf.emit(&[0x4C, 0x01, 0xD9]); // ADD RCX, R11 (dst_end)
            buf.emit(&[0x49, 0x83, 0xEB, 0x20]); // SUB R11, 32 (always >= 1)
            let top_back = buf.pos();
            buf.emit(&[0x41, 0x0F, 0x10, 0x42, 0xF0]); // MOVUPS XMM0, [R10 - 16]
            buf.emit(&[0x41, 0x0F, 0x10, 0x4A, 0xE0]); // MOVUPS XMM1, [R10 - 32]
            buf.emit(&[0x0F, 0x11, 0x41, 0xF0]); // MOVUPS [RCX - 16], XMM0
            buf.emit(&[0x0F, 0x11, 0x49, 0xE0]); // MOVUPS [RCX - 32], XMM1
            buf.emit(&[0x49, 0x83, 0xEA, 0x20]); // SUB R10, 32
            buf.emit(&[0x48, 0x83, 0xE9, 0x20]); // SUB RCX, 32
            buf.emit(&[0x49, 0x83, 0xEB, 0x20]); // SUB R11, 32
            let rel_back = top_back as i64 - (buf.pos() as i64 + 2);
            debug_assert!(i8::try_from(rel_back).is_ok());
            buf.emit(&[0x73, rel_back as i8 as u8]); // JAE top_back
            buf.emit(&[0x49, 0x83, 0xC3, 0x20]); // ADD R11, 32
            hit_jumps.push(jcc32(buf, 0x84)); // JZ done
            buf.emit(&[0x4D, 0x29, 0xDA]); // SUB R10, R11
            buf.emit(&[0x4C, 0x29, 0xD9]); // SUB RCX, R11
            to_small.push(jmp32(buf));

            // Forward: 64-byte SIMD loop, optional 32-byte step, then small tail.
            patch_rel32_to_here(buf, fwd_low);
            patch_rel32_to_here(buf, fwd_high);
            buf.emit(&[0x49, 0x83, 0xEB, 0x40]); // SUB R11, 64
            let fwd_lt64 = jcc32(buf, 0x82); // JB fwd_lt64
            let top_fwd = buf.pos();
            buf.emit(&[0x41, 0x0F, 0x10, 0x02]); // MOVUPS XMM0, [R10]
            buf.emit(&[0x41, 0x0F, 0x10, 0x4A, 0x10]); // MOVUPS XMM1, [R10 + 16]
            buf.emit(&[0x0F, 0x11, 0x01]); // MOVUPS [RCX], XMM0
            buf.emit(&[0x0F, 0x11, 0x49, 0x10]); // MOVUPS [RCX + 16], XMM1
            buf.emit(&[0x41, 0x0F, 0x10, 0x42, 0x20]); // MOVUPS XMM0, [R10 + 32]
            buf.emit(&[0x41, 0x0F, 0x10, 0x4A, 0x30]); // MOVUPS XMM1, [R10 + 48]
            buf.emit(&[0x0F, 0x11, 0x41, 0x20]); // MOVUPS [RCX + 32], XMM0
            buf.emit(&[0x0F, 0x11, 0x49, 0x30]); // MOVUPS [RCX + 48], XMM1
            buf.emit(&[0x49, 0x83, 0xC2, 0x40]); // ADD R10, 64
            buf.emit(&[0x48, 0x83, 0xC1, 0x40]); // ADD RCX, 64
            buf.emit(&[0x49, 0x83, 0xEB, 0x40]); // SUB R11, 64
            let rel_fwd = top_fwd as i64 - (buf.pos() as i64 + 2);
            debug_assert!(i8::try_from(rel_fwd).is_ok());
            buf.emit(&[0x73, rel_fwd as i8 as u8]); // JAE top_fwd
            patch_rel32_to_here(buf, fwd_lt64);
            buf.emit(&[0x49, 0x83, 0xC3, 0x40]); // ADD R11, 64
            hit_jumps.push(jcc32(buf, 0x84)); // JZ done
            buf.emit(&[0x49, 0x83, 0xFB, 0x20]); // CMP R11, 32
            to_small.push(jcc32(buf, 0x86)); // JBE small_1_to_32
                                             // 33..=63 bytes remaining: copy 32B and fall through to small_1_to_32.
            buf.emit(&[0x41, 0x0F, 0x10, 0x02]); // MOVUPS XMM0, [R10]
            buf.emit(&[0x41, 0x0F, 0x10, 0x4A, 0x10]); // MOVUPS XMM1, [R10 + 16]
            buf.emit(&[0x0F, 0x11, 0x01]); // MOVUPS [RCX], XMM0
            buf.emit(&[0x0F, 0x11, 0x49, 0x10]); // MOVUPS [RCX + 16], XMM1
            buf.emit(&[0x49, 0x83, 0xC2, 0x20]); // ADD R10, 32
            buf.emit(&[0x48, 0x83, 0xC1, 0x20]); // ADD RCX, 32
            buf.emit(&[0x49, 0x83, 0xEB, 0x20]); // SUB R11, 32
        }
    }

    // small_1_to_32: all endpoints load into registers BEFORE either store,
    // making every 1..=32 byte copy unconditionally memmove-safe for overlaps.
    for p in to_small {
        patch_rel32_to_here(buf, p);
    }
    buf.emit(&[0x49, 0x83, 0xFB, 0x10]); // CMP R11, 16
    let le16 = jcc32(buf, 0x86); // JBE le16
    buf.emit(&[0x41, 0x0F, 0x10, 0x02]); // MOVUPS XMM0, [R10]
    buf.emit(&[0x43, 0x0F, 0x10, 0x4C, 0x1A, 0xF0]); // MOVUPS XMM1, [R10 + R11 - 16]
    buf.emit(&[0x0F, 0x11, 0x01]); // MOVUPS [RCX], XMM0
    buf.emit(&[0x42, 0x0F, 0x11, 0x4C, 0x19, 0xF0]); // MOVUPS [RCX + R11 - 16], XMM1
    hit_jumps.push(jmp32(buf));

    patch_rel32_to_here(buf, le16);
    buf.emit(&[0x49, 0x83, 0xFB, 0x08]); // CMP R11, 8
    let lt8 = jcc32(buf, 0x82); // JB lt8
    buf.emit(&[0x49, 0x8B, 0x02]); // MOV RAX, [R10]
    buf.emit(&[0x4B, 0x8B, 0x54, 0x1A, 0xF8]); // MOV RDX, [R10 + R11 - 8]
    buf.emit(&[0x48, 0x89, 0x01]); // MOV [RCX], RAX
    buf.emit(&[0x4A, 0x89, 0x54, 0x19, 0xF8]); // MOV [RCX + R11 - 8], RDX
    hit_jumps.push(jmp32(buf));

    patch_rel32_to_here(buf, lt8);
    buf.emit(&[0x49, 0x83, 0xFB, 0x04]); // CMP R11, 4
    let lt4 = jcc32(buf, 0x82); // JB lt4
    buf.emit(&[0x41, 0x8B, 0x02]); // MOV EAX, [R10]
    buf.emit(&[0x43, 0x8B, 0x54, 0x1A, 0xFC]); // MOV EDX, [R10 + R11 - 4]
    buf.emit(&[0x89, 0x01]); // MOV [RCX], EAX
    buf.emit(&[0x42, 0x89, 0x54, 0x19, 0xFC]); // MOV [RCX + R11 - 4], EDX
    hit_jumps.push(jmp32(buf));

    patch_rel32_to_here(buf, lt4);
    buf.emit(&[0x49, 0x83, 0xFB, 0x02]); // CMP R11, 2
    let eq1 = jcc32(buf, 0x82); // JB eq1
    buf.emit(&[0x41, 0x0F, 0xB7, 0x02]); // MOVZX EAX, WORD [R10]
    buf.emit(&[0x43, 0x0F, 0xB7, 0x54, 0x1A, 0xFE]); // MOVZX EDX, WORD [R10 + R11 - 2]
    buf.emit(&[0x66, 0x89, 0x01]); // MOV [RCX], AX
    buf.emit(&[0x66, 0x42, 0x89, 0x54, 0x19, 0xFE]); // MOV [RCX + R11 - 2], DX
    hit_jumps.push(jmp32(buf));

    patch_rel32_to_here(buf, eq1);
    buf.emit(&[0x41, 0x8A, 0x02]); // MOV AL, [R10]
    buf.emit(&[0x88, 0x01]); // MOV [RCX], AL
}
