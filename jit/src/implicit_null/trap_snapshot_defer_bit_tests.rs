use super::{split_trap_snapshot_defer_bit, TRAP_SNAPSHOT_DEFER_BIT};

#[test]
fn the_defer_bit_round_trips_every_bci_and_leaves_plain_values_alone() {
    for bci in [0_i64, 1, 7, 65_535] {
        assert_eq!(split_trap_snapshot_defer_bit(bci), (bci, false));
        assert_eq!(
            split_trap_snapshot_defer_bit(bci | TRAP_SNAPSHOT_DEFER_BIT),
            (bci, true)
        );
        // The pads emit it as a sign-extended imm32.
        let imm = (bci | TRAP_SNAPSHOT_DEFER_BIT) as i32;
        assert!(imm > 0);
        assert_eq!(split_trap_snapshot_defer_bit(i64::from(imm)), (bci, true));
    }
    assert_eq!(split_trap_snapshot_defer_bit(-1), (-1, false));
    assert_eq!(split_trap_snapshot_defer_bit(i64::MIN), (i64::MIN, false));
}
