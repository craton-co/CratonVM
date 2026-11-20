use super::{branch_targets_except, home_reload_targets_except};
use std::collections::HashSet;

#[test]
fn empty_and_maximum_target_sets_preserve_conservative_scans() {
    let none = HashSet::new();
    let code = (usize::MAX as u64).to_le_bytes();
    let wanted = HashSet::from([usize::MAX]);
    for scan in [home_reload_targets_except, branch_targets_except] {
        assert!(scan(&code, 0, &none, &none).is_empty());
        assert_eq!(scan(&code, 0, &wanted, &none), wanted);
    }
}
