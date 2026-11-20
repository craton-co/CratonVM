use super::*;

fn standing(rows: &[(usize, bool)]) -> HashMap<usize, bool> {
    rows.iter().copied().collect()
}

fn set(bcis: &[usize]) -> HashSet<usize> {
    bcis.iter().copied().collect()
}

#[test]
fn j27_irrows_only_undominated_anchors_are_counted_in_bci_order() {
    let found = undominated_anchors(
        &standing(&[(9, false), (3, true), (5, false), (7, true)]),
        &set(&[3, 9]),
    );
    assert_eq!(found.bcis, vec![5, 9]);
    assert_eq!(found.flat_transfer, vec![9], "bci 3 transfers but its anchor is honest");
}

#[test]
fn j27_irrows_every_honest_anchor_counts_nothing() {
    let found = undominated_anchors(&standing(&[(1, true), (4, true)]), &set(&[1, 4]));
    assert_eq!(found, UndominatedAnchors::default());
}

#[test]
fn j27_irrows_the_refusal_needs_the_switch_and_a_flat_transfer() {
    let found = undominated_anchors(&standing(&[(5, false), (9, false)]), &set(&[9]));
    assert!(undominated_anchor_refusal(&found, false).is_none(), "opt-in");
    let bailout = undominated_anchor_refusal(&found, true).expect("bci 9 is read by a transfer");
    assert!(
        matches!(bailout.reason, BailoutReason::UnsupportedShape(_)),
        "{bailout:?}"
    );
    assert!(format!("{bailout:?}").contains("bci 9"), "{bailout:?}");
    // An undominated anchor no transfer reads is counted, never refused.
    let unread = undominated_anchors(&standing(&[(5, false)]), &set(&[9]));
    assert_eq!(unread.bcis, vec![5]);
    assert!(undominated_anchor_refusal(&unread, true).is_none());
}
