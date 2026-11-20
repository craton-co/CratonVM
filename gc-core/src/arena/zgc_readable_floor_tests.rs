use super::*;
use crate::reservation::GRANULE;

#[test]
fn readable_give_back_never_admits_reset_of_minimum_commitment() {
    let mut arena = Arena::new(8 * GRANULE);
    arena.commit_initial_prefix(4 * GRANULE);
    arena.set_uncommit_policy(4 * GRANULE, true, std::time::Duration::ZERO);
    arena.set_readable_give_back(true);
    let committed = arena.committed_bytes();
    assert_eq!(arena.decommit_unused_above(0), 0);
    assert_eq!(arena.committed_bytes(), committed);
    // Even Windows's reset/recommit-failure path cannot release the floor:
    // no reset is attempted when the commitment equals the minimum.
    if arena.data.commit_bits().is_some() {
        assert_eq!(arena.bytes_reset, 0);
    }
}
