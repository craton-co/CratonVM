use super::*;

#[test]
fn the_abs64_call_decodes_its_immediate() {
    let mut code = vec![0x90u8, 0x90];
    code.extend_from_slice(&[0x48, 0xB8]);
    code.extend_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
    code.extend_from_slice(&[0xFF, 0xD0]);
    let ret = code.len();
    assert_eq!(abs64_call_imm_offset(&code, ret), Some(4));
}

#[test]
fn other_sequences_are_not_abs64_calls() {
    // A mismatched register: MOV R11, imm64 ; CALL R10.
    let mut code = vec![0x49u8, 0xBB];
    code.extend_from_slice(&[0; 8]);
    code.extend_from_slice(&[0x41, 0xFF, 0xD2]);
    assert_eq!(abs64_call_imm_offset(&code, code.len()), None);
    // E8 rel32, and too short a prefix.
    let e8 = [0xE8u8, 1, 2, 3, 4];
    assert_eq!(abs64_call_imm_offset(&e8, 5), None);
    assert_eq!(abs64_call_imm_offset(&[0xFF, 0xD0], 2), None);
    // Past the end of the code.
    assert_eq!(abs64_call_imm_offset(&e8, 20), None);
}

#[test]
fn the_r11_abs64_call_decodes_its_immediate() {
    let mut code = vec![0x90u8];
    code.extend_from_slice(&[0x49, 0xBB]);
    code.extend_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
    code.extend_from_slice(&[0x41, 0xFF, 0xD3]);
    let ret = code.len();
    assert_eq!(abs64_call_imm_offset(&code, ret), Some(3));
}

#[test]
fn retire_cell_call_addr_decodes_both_mov_r11_forms_before_abs64() {
    // 1. 64-bit cell_addr (`49 BB <imm64>`) + Windows-style 32-bit helper tail
    // (`4c 8d 45 90 41 b9 03 00 00 00 41 bb 00 41 c1 d0 41 ff d3`).
    let cell64: u64 = 0x0000_7f88_1122_3340;
    let tail_imm32: [u8; 16] = [
        0x4c, 0x8d, 0x45, 0x90, 0x41, 0xb9, 0x03, 0x00, 0x00, 0x00, 0x41, 0xbb, 0x00, 0x41,
        0xc1, 0xd0,
    ];
    let mut code64 = vec![0x90u8];
    code64.extend_from_slice(&[0x49, 0xBB]);
    code64.extend_from_slice(&cell64.to_le_bytes());
    code64.extend_from_slice(&[
        0x4D, 0x8B, 0x9B, 0x00, 0x00, 0x00, 0x00, 0x4D, 0x85, 0xDB, 0x0F, 0x85,
    ]);
    code64.extend_from_slice(&(tail_imm32.len() as i32).to_le_bytes());
    code64.extend_from_slice(&tail_imm32);
    code64.extend_from_slice(&[0x41, 0xFF, 0xD3]);
    assert_eq!(
        retire_cell_call_addr(&code64, code64.len()),
        Some(cell64 as usize)
    );

    // 2. 32-bit cell_addr (`41 BB <imm32>`) + Linux PIE 64-bit helper tail
    // (`49 BB <invoke_dispatch> 41 FF D3`), which `abs64_call_imm_offset`
    // would otherwise misidentify as a direct `MOV R11, imm64 ; CALL R11`.
    let cell32: u32 = 0x3344_5560;
    let mut tail_imm64 = vec![0x4c, 0x8d, 0x45, 0x90, 0x49, 0xbb];
    tail_imm64.extend_from_slice(&0x0000_5555_6677_8899u64.to_le_bytes());
    let mut code32 = vec![0x48u8, 0x83, 0xEC, 0x20];
    code32.extend_from_slice(&[0x41, 0xBB]);
    code32.extend_from_slice(&cell32.to_le_bytes());
    code32.extend_from_slice(&[
        0x4D, 0x8B, 0x9B, 0x00, 0x00, 0x00, 0x00, 0x4D, 0x85, 0xDB, 0x0F, 0x85,
    ]);
    code32.extend_from_slice(&(tail_imm64.len() as i32).to_le_bytes());
    code32.extend_from_slice(&tail_imm64);
    code32.extend_from_slice(&[0x41, 0xFF, 0xD3]);
    assert_eq!(
        retire_cell_call_addr(&code32, code32.len()),
        Some(cell32 as usize)
    );
    // Plain `MOV R11, imm64 ; CALL R11` without the retire-cell guard is not matched.
    let mut plain = vec![0x49u8, 0xBB];
    plain.extend_from_slice(&cell64.to_le_bytes());
    plain.extend_from_slice(&[0x41, 0xFF, 0xD3]);
    assert_eq!(retire_cell_call_addr(&plain, plain.len()), None);
}


