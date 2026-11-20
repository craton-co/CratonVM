use super::*;

#[test]
fn the_raw_announce_is_the_single_pass_sequence_byte_for_byte() {
    for (cursor_off, end_off) in [(0xF0i32, 0xF8i32), (0x1000, 0x1008), (8, 16)] {
        let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        let a0 = c.buf.pos();
        let (done_a, helper_a) = c.emit_zgc_inline_announce(
            cursor_off,
            end_off,
            ZgcAnnounceCommit::AlreadyCommitted,
        );
        let a = c.buf.as_slice()[a0..c.buf.pos()].to_vec();

        let mut d = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        let b0 = d.buf.pos();
        let (done_b, helper_b) = emit_zgc_inline_announce_raw(&mut d.buf, cursor_off, end_off);
        let b = d.buf.as_slice()[b0..d.buf.pos()].to_vec();

        assert_eq!(a, b, "cursor_off={cursor_off:#x} end_off={end_off:#x}");
        assert_eq!(done_a - a0, done_b - b0, "the `done` patch offsets agree");
        let rel = |v: &[usize], base: usize| v.iter().map(|p| p - base).collect::<Vec<_>>();
        assert_eq!(
            rel(&helper_a, a0),
            rel(&helper_b, b0),
            "helper patches agree"
        );
    }
}

