//! Round 16, lane mega: the receiver policy of the polymorphic guarded
//! splice (`ReceiverEvidence::polymorphic_set`).
use super::*;

fn set(rows: &[(u32, u32)], max: usize) -> Option<Vec<u32>> {
    let counts: ReceiverCounts = rows.iter().copied().collect();
    ReceiverEvidence::of(&counts).polymorphic_set(100, max, 80, 10, 90)
}

#[test]
fn three_or_four_classes_are_named_best_first() {
    // `R16Bench poly`: 50 / 25 / 25.
    assert_eq!(
        set(&[(659, 500), (660, 250), (661, 250)], 4),
        Some(vec![659, 660, 661])
    );
    // Ties rank by ascending class id.
    assert_eq!(
        set(&[(7, 250), (5, 250), (9, 250), (3, 250)], 4),
        Some(vec![3, 5, 7, 9])
    );
    // Only as many as it takes to reach the total.
    assert_eq!(
        set(&[(1, 400), (2, 300), (3, 250), (4, 50)], 4),
        Some(vec![1, 2, 3])
    );
}

#[test]
fn everything_else_names_nothing() {
    assert_eq!(
        set(&[(1, 40), (2, 20), (3, 20)], 4),
        None,
        "too few samples"
    );
    assert_eq!(
        set(&[(1, 900), (2, 50), (3, 50)], 4),
        None,
        "the one-class splice's site"
    );
    assert_eq!(set(&[(1, 500), (2, 500)], 4), None, "a pair is bimorphic");
    assert_eq!(
        set(&[(1, 300), (2, 300), (3, 200), (4, 200)], 3),
        None,
        "three classes hold 80 percent, under the total"
    );
    assert_eq!(
        set(
            &[
                (1, 400),
                (2, 300),
                (3, 50),
                (4, 50),
                (5, 50),
                (6, 50),
                (7, 50),
                (8, 50)
            ],
            8
        ),
        None,
        "the third class is under the per-class share"
    );
    assert_eq!(
        set(&[(0, 500), (2, 250), (3, 250)], 4),
        None,
        "the unknown class"
    );
    assert_eq!(
        set(&[(1, 500), (2, 250), (3, 250)], 2),
        None,
        "a cap under three"
    );
}
