use super::*;

const SEG: u8 = 0x65;
const DISP: u32 = 0x1480;

/// Offset of the `JNZ rel8` in the sequence: prefix + MOV (4 + 4) + TEST (3).
const JNZ_AT: usize = 1 + 4 + 4 + 3;

#[test]
fn j27_loopcg_the_mirror_load_and_null_test_come_first() {
    let b = poll_word_select_bytes(0x1000, SEG, DISP, 0x2000, true);
    assert_eq!(b[0], SEG);
    assert_eq!(&b[1..5], &[0x4C, 0x8B, 0x1C, 0x25]);
    assert_eq!(&b[5..9], &DISP.to_le_bytes());
    assert_eq!(&b[9..12], &[0x4D, 0x85, 0xDB]);
    assert_eq!(b[JNZ_AT], 0x75, "JNZ rel8 over the fallback");
}

/// In reach: the fallback is a `LEA` whose displacement names the flag
/// from the end of the `LEA`, and the `JNZ` lands on the one byte test.
#[test]
fn j27_loopcg_in_reach_the_fallback_is_a_rip_lea_of_the_flag() {
    let here = 0x7000_0000usize;
    let flag = here + 0x10_0000;
    let b = poll_word_select_bytes(here, SEG, DISP, flag, true);
    let lea = JNZ_AT + 2;
    assert_eq!(b[JNZ_AT + 1], 7);
    assert_eq!(&b[lea..lea + 3], &[0x4C, 0x8D, 0x1D]);
    let disp = i32::from_le_bytes([b[lea + 3], b[lea + 4], b[lea + 5], b[lea + 6]]);
    let next = here + lea + 7;
    assert_eq!((next as i64 + i64::from(disp)) as usize, flag);
    let have = JNZ_AT + 2 + usize::from(b[JNZ_AT + 1]);
    assert_eq!(&b[have..], &[0x41, 0xF6, 0x03, 0xFF]);
    assert_eq!(b.len(), have + 4);
}

/// Out of reach, or with the RIP poll killed: `MOV R11, imm64 flag`.
#[test]
fn j27_loopcg_out_of_reach_or_rip_off_the_fallback_is_an_imm64() {
    let here = 0x1000usize;
    let far = 0x7FFF_0000_0000usize;
    for (flag, rip_ok) in [(far, true), (here + 0x100, false)] {
        let b = poll_word_select_bytes(here, SEG, DISP, flag, rip_ok);
        assert_eq!(b[JNZ_AT + 1], 10);
        let mov = JNZ_AT + 2;
        assert_eq!(&b[mov..mov + 2], &[0x49, 0xBB]);
        let mut imm = [0u8; 8];
        imm.copy_from_slice(&b[mov + 2..mov + 10]);
        assert_eq!(u64::from_le_bytes(imm), flag as u64);
        assert_eq!(&b[mov + 10..], &[0x41, 0xF6, 0x03, 0xFF]);
    }
}

