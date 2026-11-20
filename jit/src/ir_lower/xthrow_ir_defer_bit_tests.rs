/// THROW20-6: whatever the switches say, the pad's `bci | bit` reads back
/// in the setters as the bci itself, deferred exactly when the bit is set.
#[test]
fn an_ir_pad_bci_with_the_defer_bit_strips_back_to_the_bci() {
    let bit = super::ir_defer_trap_snapshot_bit();
    assert!(bit == 0 || bit == crate::implicit_null::TRAP_SNAPSHOT_DEFER_BIT as u64);
    for bci in [0_usize, 1, 9, 4_096, 65_535] {
        let raw = bci as u64 | bit;
        // The pads load it as an imm64 and the setters take it as an `i64`.
        let (stripped, deferred) =
            crate::implicit_null::split_trap_snapshot_defer_bit(raw as i64);
        assert_eq!(stripped, bci as i64);
        assert_eq!(deferred, bit != 0);
    }
}
