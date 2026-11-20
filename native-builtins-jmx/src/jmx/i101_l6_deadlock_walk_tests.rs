//! Interpreter round i5 wave 101, lane L6: [`super::hotspot_deadlock_walk`]
//! answers what HotSpot's `find_deadlocks_at_safepoint` does.
use super::hotspot_deadlock_walk;

#[test]
fn a_two_thread_cycle_is_reported_in_walk_order() {
    let nodes: &[(i64, Option<i64>)] = &[(1, None), (5, Some(6)), (6, Some(5))];
    assert_eq!(hotspot_deadlock_walk(nodes), vec![5, 6]);
}

#[test]
fn a_self_loop_is_ignored() {
    let nodes: &[(i64, Option<i64>)] = &[(1, None), (7, Some(7))];
    assert!(hotspot_deadlock_walk(nodes).is_empty());
}

#[test]
fn a_lead_in_walked_first_is_reported_with_its_cycle() {
    // 4 waits for 5; 5 and 6 are deadlocked. 4 is walked first.
    let nodes: &[(i64, Option<i64>)] = &[(4, Some(5)), (5, Some(6)), (6, Some(5))];
    assert_eq!(hotspot_deadlock_walk(nodes), vec![4, 5, 6]);
    // Walked after the cycle, the lead-in is not.
    let nodes: &[(i64, Option<i64>)] = &[(5, Some(6)), (6, Some(5)), (4, Some(5))];
    assert_eq!(hotspot_deadlock_walk(nodes), vec![5, 6]);
}

#[test]
fn a_chain_without_a_cycle_reports_nothing() {
    let nodes: &[(i64, Option<i64>)] = &[(1, Some(2)), (2, Some(3)), (3, None), (9, Some(42))];
    assert!(hotspot_deadlock_walk(nodes).is_empty());
}
