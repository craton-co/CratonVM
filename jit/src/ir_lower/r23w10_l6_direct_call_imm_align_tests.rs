use super::*;

#[test]
fn the_pad_puts_the_imm64_on_an_eight_byte_boundary() {
    for at in 0x1000usize..0x1010 {
        let pad = direct_call_imm_align_pad(at);
        assert!(pad < 8);
        assert_eq!((at + pad + 2) % 8, 0, "at {at:#x}");
    }
    assert_eq!(direct_call_imm_align_pad(0x1006), 0);
    assert_eq!(direct_call_imm_align_pad(0x1007), 7);
}

