// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

#[test]
fn append_int_reuses_its_capacity_size_without_a_second_digit_scan() {
    use iced_x86::{Decoder, DecoderOptions, Mnemonic, Register};
    let mut buf = ExecutableBuffer::new(4096).unwrap();
    emit_sb_append_int_latin1(&mut buf, &mut Vec::new());
    let mut decoder = Decoder::new(64, buf.as_slice(), DecoderOptions::NONE);
    let mut scans = 0;
    while decoder.can_decode() {
        let i = decoder.decode();
        if i.mnemonic() == Mnemonic::Imul
            && i.op0_register() == Register::RDX
            && i.op1_register() == Register::RDX
            && i.immediate8() == 10
        {
            scans += 1;
        }
    }
    assert_eq!(
        scans, 1,
        "append(int) must reuse the size already checked against capacity"
    );
}

#[test]
fn known_end_append_preserves_text_bounds_cursor_count_and_surviving_register() {
    let mut buf = ExecutableBuffer::new(16384).unwrap();
    buf.emit(&[0x57]); // PUSH RDI: record pointer across the leaf emitter on both ABIs.
    #[cfg(windows)]
    mov_r64_r64(&mut buf, 7, RCX);
    buf.emit(&[0x48, 0x8b, 0x0f]); // MOV RCX,[RDI]
    buf.emit(&[0x44, 0x8b, 0x5f, 8]); // MOV R11D,[RDI+8]
    buf.emit(&[0x44, 0x8b, 0x57, 16]); // MOV R10D,[RDI+16]
    let mut decline = Vec::new();
    emit_sb_append_int_latin1(&mut buf, &mut decline);
    mov_mem_r64(&mut buf, 7, R8, 24);
    mov_mem_r64(&mut buf, 7, R9, 32);
    mov_mem_r64(&mut buf, 7, R11, 40);
    mov_r64_r64(&mut buf, RAX, R9);
    buf.emit(&[0x5f, 0xc3]);
    for patch in decline {
        patch_here(&mut buf, patch);
    }
    mov_imm32_sx(&mut buf, RAX, -1);
    buf.emit(&[0x5f, 0xc3]);
    buf.finalize();
    // SAFETY: the finalized fixture emits this C ABI and its executable buffer remains live.
    let run: unsafe extern "C" fn(*mut u64) -> i64 = unsafe { std::mem::transmute(buf.as_ptr()) };
    let mut ints = vec![i32::MIN, i32::MAX, 0, -1, 1];
    for power in (0..=9).map(|n| 10_i64.pow(n)) {
        for delta in -1..=1 {
            for sign in [-1, 1] {
                if let Ok(i) = i32::try_from(sign * (power + delta)) {
                    ints.push(i);
                }
            }
        }
    }
    let mut seed = 0x7a49_c213_u32;
    for _ in 0..256 {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        ints.push(seed as i32);
    }
    for i in ints {
        let text = i.to_string();
        for prefix in [0_usize, 3] {
            let end = prefix + text.len();
            for cap in [end - 1, end, end + 7] {
                let mut array = vec![0xeeee_eeee_eeee_eeee_u64; 32];
                let data = cratonvm_types::HEADER_SIZE;
                // SAFETY: the exclusive byte view covers only this live, initialized u64 buffer.
                let bytes = unsafe {
                    std::slice::from_raw_parts_mut(array.as_mut_ptr().cast::<u8>(), array.len() * 8)
                };
                let len = cratonvm_types::ARRAY_LENGTH_OFFSET;
                bytes[len..len + 4].copy_from_slice(&(cap as u32).to_le_bytes());
                let mut expected = bytes.to_vec();
                let addr = array.as_mut_ptr() as usize;
                let mut state = [
                    addr as u64,
                    prefix as u64,
                    i as u32 as u64,
                    0x5555,
                    0x6666,
                    0x7777,
                    0x8888,
                    0x9999,
                ];
                let mut expected_state = state;
                let answer = if cap < end {
                    -1
                } else {
                    expected[data + prefix..data + end].copy_from_slice(text.as_bytes());
                    expected_state[3] = (addr + data + end) as u64;
                    expected_state[4] = end as u64;
                    expected_state[5] = prefix as u64;
                    end as i64
                };
                // The supplied array and record remain live for this non-collecting leaf.
                assert_eq!(
                    // SAFETY: the live frame and object buffers satisfy this non-collecting leaf's C ABI.
                    unsafe { run(state.as_mut_ptr()) },
                    answer,
                    "i={i} prefix={prefix} cap={cap}"
                );
                // SAFETY: the byte view stays within the live, initialized array and no writer runs.
                let after = unsafe {
                    std::slice::from_raw_parts(array.as_ptr().cast::<u8>(), array.len() * 8)
                };
                assert_eq!(
                    after, expected,
                    "exact array image including declined writes"
                );
                assert_eq!(state, expected_state, "end/count and R11 contract");
            }
        }
    }
}
