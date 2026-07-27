// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Runtime-sensitive x86-64 lowering shared by the baseline and optimizing JITs.
//!
//! The two tiers deliberately retain different graph/bytecode construction and
//! optimization policies.  They must not, however, grow independent encodings
//! for calls, allocation, locking, barriers, or dispatch.  This module is the
//! common machine-lowering seam for those operations.

use crate::{ExecutableBuffer, JitPICSlot};

const RAX: u8 = 0;
const RCX: u8 = 1;
const RDX: u8 = 2;
const R10: u8 = 10;
const R11: u8 = 11;

#[cfg(target_os = "windows")]
const ENTRY_ABI_REGS: &[u8] = &[1, 2, 8, 9];
#[cfg(not(target_os = "windows"))]
const ENTRY_ABI_REGS: &[u8] = &[7, 6, 2, 1, 8, 9];

#[inline]
fn rex_w(buf: &mut ExecutableBuffer, reg: u8, index: u8, base: u8) {
    let mut rex = 0x48;
    if reg >= 8 {
        rex |= 0x04;
    }
    if index >= 8 {
        rex |= 0x02;
    }
    if base >= 8 {
        rex |= 0x01;
    }
    buf.emit_byte(rex);
}

#[inline]
fn emit_mov_imm64(buf: &mut ExecutableBuffer, reg: u8, value: u64) {
    buf.emit_byte(0x48 | u8::from(reg >= 8));
    buf.emit_byte(0xB8 + (reg & 7));
    buf.emit(&value.to_le_bytes());
}

#[inline]
fn emit_load_frame(buf: &mut ExecutableBuffer, reg: u8, offset: i32) {
    rex_w(buf, reg, 0, 5);
    buf.emit_byte(0x8B);
    buf.emit_byte(0x80 | ((reg & 7) << 3) | 5);
    buf.emit(&(-offset).to_le_bytes());
}

#[inline]
fn emit_jcc(buf: &mut ExecutableBuffer, cc: u8) -> usize {
    buf.emit(&[0x0F, cc]);
    let patch = buf.pos();
    buf.emit(&[0; 4]);
    patch
}

#[inline]
fn emit_jmp(buf: &mut ExecutableBuffer) -> usize {
    buf.emit_byte(0xE9);
    let patch = buf.pos();
    buf.emit(&[0; 4]);
    patch
}

#[inline]
pub(crate) fn patch_rel32_to_here(buf: &mut ExecutableBuffer, patch: usize) {
    let displacement = (buf.pos() as i64) - (patch as i64 + 4);
    if let Ok(displacement) = i32::try_from(displacement) {
        let _ = buf.try_patch_i32(patch, displacement);
    } else {
        buf.mark_overflowed();
    }
}

fn emit_marshal(
    buf: &mut ExecutableBuffer,
    context_offset: i32,
    arg_offsets: &[i32],
    needs_context: bool,
) {
    let shift = usize::from(needs_context);
    if needs_context {
        emit_load_frame(buf, ENTRY_ABI_REGS[0], context_offset);
    }
    for (index, &offset) in arg_offsets.iter().enumerate() {
        emit_load_frame(buf, ENTRY_ABI_REGS[index + shift], offset);
    }
}

#[inline]
fn emit_mov_reg(buf: &mut ExecutableBuffer, dst: u8, src: u8) {
    rex_w(buf, src, 0, dst);
    buf.emit_byte(0x89);
    buf.emit_byte(0xC0 | ((src & 7) << 3) | (dst & 7));
}

#[inline]
fn emit_call_absolute(buf: &mut ExecutableBuffer, target: usize) {
    emit_mov_imm64(buf, RAX, target as u64);
    buf.emit(&[0xFF, 0xD0]);
}

fn emit_post_call_frame_republish(buf: &mut ExecutableBuffer, frame_record: usize) {
    if frame_record == 0 {
        return;
    }
    buf.emit_byte(0x50); // PUSH RAX (preserve Java return)
    #[cfg(target_os = "windows")]
    const RESERVE: u8 = 40; // shadow space + alignment after PUSH
    #[cfg(not(target_os = "windows"))]
    const RESERVE: u8 = 8; // restore 16-byte call-site alignment
    buf.emit(&[0x48, 0x83, 0xEC, RESERVE]); // SUB RSP,reserve
    emit_mov_reg(buf, ENTRY_ABI_REGS[0], 5); // ARG0 = RBP
    emit_call_absolute(buf, frame_record);
    buf.emit(&[0x48, 0x83, 0xC4, RESERVE]); // ADD RSP,reserve
    buf.emit_byte(0x58); // POP RAX
}

/// Emit the canonical object-allocation runtime stub used by both JIT tiers.
///
/// The runtime helper owns class initialization, compact-layout sizing, the
/// per-thread TLAB fast path, GC retry, and the zero-on-failure convention.
/// Keeping this ABI sequence here prevents the baseline and optimizing
/// backends from drifting on argument order or post-call frame publication.
pub(crate) fn emit_new_object_stub(
    buf: &mut ExecutableBuffer,
    context_offset: i32,
    target: usize,
    class_id: u32,
    num_fields: usize,
    frame_record: usize,
) {
    emit_load_frame(buf, ENTRY_ABI_REGS[0], context_offset);
    emit_mov_imm64(buf, ENTRY_ABI_REGS[1], u64::from(class_id));
    emit_mov_imm64(buf, ENTRY_ABI_REGS[2], num_fields as u64);
    emit_call_absolute(buf, target);
    emit_post_call_frame_republish(buf, frame_record);
}

/// Emit a VM/object runtime call used for monitor enter/exit.
///
/// Both operands are canonical frame slots, so the call remains valid after
/// register allocation and at GC safepoints. The helper returns a non-sentinel
/// value on success and `i64::MIN` after publishing a pending Java exception.
pub(crate) fn emit_monitor_stub(
    buf: &mut ExecutableBuffer,
    context_offset: i32,
    object_offset: i32,
    target: usize,
    frame_record: usize,
) {
    emit_load_frame(buf, ENTRY_ABI_REGS[0], context_offset);
    emit_load_frame(buf, ENTRY_ABI_REGS[1], object_offset);
    emit_call_absolute(buf, target);
    emit_post_call_frame_republish(buf, frame_record);
}

/// Emit the compact two-way hashed/vtable dispatch stub shared by both tiers.
///
/// `arg_offsets[0]` is the receiver.  The generated code hashes its class id
/// into one of eight sets, checks the two immutable-published ways, marshals the
/// selected compiled entry ABI, and calls it directly.  A null receiver, empty
/// entry, or hash miss falls through to the caller's resolving helper.
///
/// Returned rel32 sites are successful-call jumps which the caller patches to
/// its post-call continuation after emitting the slow helper.  All miss
/// branches are patched inside this function to the fall-through position.
pub(crate) fn emit_hashed_vtable_stub(
    buf: &mut ExecutableBuffer,
    pic: usize,
    context_offset: i32,
    arg_offsets: &[i32],
    frame_record: usize,
) -> Vec<usize> {
    if arg_offsets.is_empty()
        || arg_offsets.len() + 1 > ENTRY_ABI_REGS.len()
        || JitPICSlot::MEGA_SET_SHIFT >= 32
    {
        return Vec::new();
    }

    let mut miss_patches = Vec::with_capacity(6);
    let mut done_patches = Vec::with_capacity(2);

    // receiver -> RAX, null -> resolving helper, class id -> EDX.
    emit_load_frame(buf, RAX, arg_offsets[0]);
    buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
    miss_patches.push(emit_jcc(buf, 0x84)); // JZ slow
    buf.emit(&[0x8B, 0x10]); // MOV EDX,[RAX]

    // ECX = ((class_id * golden-ratio hash) >> shift) * 2.
    buf.emit(&[0x69, 0xCA]); // IMUL ECX,EDX,imm32
    buf.emit(&JitPICSlot::MEGA_HASH_MULTIPLIER.to_le_bytes());
    buf.emit(&[0xC1, 0xE9, JitPICSlot::MEGA_SET_SHIFT]); // SHR ECX,shift
    buf.emit(&[0xD1, 0xE1]); // SHL ECX,1
    emit_mov_imm64(buf, R10, pic as u64);

    for way in 0..2 {
        if way == 1 {
            buf.emit(&[0xFF, 0xC1]); // INC ECX
        }

        // CMP EDX,[R10 + RCX*4 + mega_class_ids]
        buf.emit(&[0x41, 0x3B, 0x94, 0x8A]);
        buf.emit(&(JitPICSlot::MEGA_CLASS_IDS_OFFSET as i32).to_le_bytes());
        let next_or_miss = emit_jcc(buf, 0x85); // JNE

        // MOV R11,[R10 + RCX*8 + mega_entry_ptrs]; zero -> slow.
        buf.emit(&[0x4D, 0x8B, 0x9C, 0xCA]);
        buf.emit(&(JitPICSlot::MEGA_ENTRY_PTRS_OFFSET as i32).to_le_bytes());
        buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11,R11
        miss_patches.push(emit_jcc(buf, 0x84));

        // Select the compiled entry ABI from the parallel byte array.
        buf.emit(&[0x41, 0x80, 0xBC, 0x0A]);
        buf.emit(&(JitPICSlot::MEGA_NEEDS_CONTEXT_OFFSET as i32).to_le_bytes());
        buf.emit_byte(0);
        let no_context = emit_jcc(buf, 0x84);
        emit_marshal(buf, context_offset, arg_offsets, true);
        let call = emit_jmp(buf);
        patch_rel32_to_here(buf, no_context);
        emit_marshal(buf, context_offset, arg_offsets, false);
        patch_rel32_to_here(buf, call);

        // R11 is deliberately outside both platform argument-register sets,
        // so marshalling cannot clobber the target loaded above.
        buf.emit(&[0x41, 0xFF, 0xD3]); // CALL R11
        emit_post_call_frame_republish(buf, frame_record);
        done_patches.push(emit_jmp(buf));

        if way == 0 {
            patch_rel32_to_here(buf, next_or_miss);
        } else {
            miss_patches.push(next_or_miss);
        }
    }

    for patch in miss_patches {
        patch_rel32_to_here(buf, patch);
    }
    done_patches
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_maps_every_class_to_a_valid_two_way_set() {
        for class_id in 1..100_000u32 {
            let base = JitPICSlot::mega_base_index(class_id);
            assert_eq!(base & 1, 0);
            assert!(base + 1 < crate::JIT_MEGA_ENTRIES);
        }
    }

    #[test]
    fn object_allocation_stub_embeds_runtime_target_and_immediates() {
        let target = 0x1122_3344_5566_7788usize;
        let mut buf = ExecutableBuffer::new(256).expect("buffer");
        emit_new_object_stub(&mut buf, 24, target, 0xAABB_CCDD, 17, 0);
        let bytes = buf.as_slice();
        assert!(bytes
            .windows(8)
            .any(|window| window == target.to_le_bytes()));
        assert!(bytes
            .windows(8)
            .any(|window| window == u64::from(0xAABB_CCDDu32).to_le_bytes()));
        assert!(bytes
            .windows(8)
            .any(|window| window == 17u64.to_le_bytes()));
        assert!(bytes.ends_with(&[0xFF, 0xD0]));
    }

    #[test]
    fn monitor_stub_embeds_target_and_loads_both_frame_operands() {
        let target = 0x8877_6655_4433_2211usize;
        let mut buf = ExecutableBuffer::new(192).expect("buffer");
        emit_monitor_stub(&mut buf, 24, 40, target, 0);
        let bytes = buf.as_slice();
        assert!(bytes
            .windows(8)
            .any(|window| window == target.to_le_bytes()));
        assert_eq!(bytes.iter().filter(|&&byte| byte == 0x8B).count(), 2);
        assert!(bytes.ends_with(&[0xFF, 0xD0]));
    }
}
