use super::*;

#[test]
fn the_padded_rel32_lies_in_one_eight_byte_word() {
    for at in 0x2000usize..0x2010 {
        let pad = sp_patchable_call_pad(at);
        assert!(pad < 4);
        let disp = at + pad + 1;
        assert_eq!(disp % 4, 0, "at {at:#x}");
        assert!((disp & 7) + 4 <= 8, "the rebind's one-word rule holds at {at:#x}");
    }
}

