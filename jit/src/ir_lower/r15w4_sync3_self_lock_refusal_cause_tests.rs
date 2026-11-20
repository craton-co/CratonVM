use super::SelfLockRefusal;
use crate::ir::{
    SYNC_CENSUS_IR_SELF_LOCK_ACCEPTED, SYNC_CENSUS_IR_SELF_LOCK_FELL_THROUGH,
    SYNC_CENSUS_IR_SELF_LOCK_GATE_DECLINED, SYNC_CENSUS_IR_SELF_LOCK_REFUSED,
    SYNC_SPLICE_CENSUS_ROWS as ROWS,
};

/// Round 15 wave 4 (lane sync3, SY2-6): every refusal cause has its own
/// row, named after it, right after the total; the three outcome rows
/// follow, and nothing is left unnamed.
#[test]
fn every_refusal_cause_counts_on_its_own_named_row() {
    assert_eq!(
        ROWS[SYNC_CENSUS_IR_SELF_LOCK_REFUSED],
        "ir-self-locking-refused"
    );
    let mut rows: Vec<usize> = Vec::new();
    for cause in SelfLockRefusal::ALL {
        let row = cause.census_row();
        assert_eq!(
            ROWS[row],
            format!("ir-self-locking-refused-{}", cause.name())
        );
        assert!(!rows.contains(&row), "{cause:?} shares a row");
        rows.push(row);
    }
    assert_eq!(
        rows.first().copied(),
        Some(SYNC_CENSUS_IR_SELF_LOCK_REFUSED + 1)
    );
    assert_eq!(
        ROWS[SYNC_CENSUS_IR_SELF_LOCK_GATE_DECLINED],
        "ir-self-locking-gate-declined"
    );
    assert_eq!(
        ROWS[SYNC_CENSUS_IR_SELF_LOCK_ACCEPTED],
        "ir-self-locking-accepted"
    );
    assert_eq!(
        ROWS[SYNC_CENSUS_IR_SELF_LOCK_FELL_THROUGH],
        "ir-self-locking-candidate-fell-through"
    );
    assert_eq!(ROWS.len(), SYNC_CENSUS_IR_SELF_LOCK_FELL_THROUGH + 1);
}
