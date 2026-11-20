use super::{snapshot_values_lost, NodeId, NO_NODE};

#[test]
fn only_a_live_slot_reading_no_node_is_a_lost_value() {
    let wide = |v: NodeId| v == 7;
    let none_dead = |_: usize| false;
    assert!(!snapshot_values_lost(&[1, 2, 3], &none_dead, &wide));
    assert!(snapshot_values_lost(&[1, NO_NODE, 3], &none_dead, &wide));
    // A dead slot may read anything: the resumed frame cannot observe it.
    assert!(!snapshot_values_lost(&[1, NO_NODE, 3], &|i| i == 1, &wide));
    // The upper half of a long or double the slot before names.
    assert!(!snapshot_values_lost(&[7, NO_NODE, 3], &none_dead, &wide));
    assert!(snapshot_values_lost(&[NO_NODE], &none_dead, &wide));
    assert!(!snapshot_values_lost(&[], &none_dead, &wide));
}
