use super::*;

const MIB: usize = 1024 * 1024;

/// With a floor, nothing below it goes back and everything above it that
/// the unfloored give-back would release still does; the floor 0 is the
/// two unfloored calls exactly.
#[test]
fn a_floored_give_back_keeps_the_prefix_and_releases_the_rest() {
    let g = crate::reservation::GRANULE;
    let mut floored = Arena::new(16 * MIB);
    if floored.commit_bits().is_none() {
        // The wholly committed fallback store releases nothing.
        return;
    }
    let committed = floored.commit_initial_prefix(12 * MIB);
    assert_eq!(committed, 12 * MIB);
    let floor = 2 * g;
    let released = floored.decommit_unused_above(floor);
    assert_eq!(
        floored.committed_bytes(),
        floor,
        "the floor's granules stay committed, the rest of the prefix goes back"
    );
    assert_eq!(released, 12 * MIB - floor, "what went back");

    // A floor that does not sit on a granule boundary keeps the granule it
    // lands in (inward rounding of the released span).
    let mut straddled = Arena::new(16 * MIB);
    let _ = straddled.commit_initial_prefix(12 * MIB);
    let _ = straddled.decommit_unused_above(g + 8);
    assert_eq!(straddled.committed_bytes(), 2 * g);

    let mut unfloored = Arena::new(16 * MIB);
    let _ = unfloored.commit_initial_prefix(12 * MIB);
    let mut split = Arena::new(16 * MIB);
    let _ = split.commit_initial_prefix(12 * MIB);
    let whole = unfloored.decommit_unused_above(0);
    let parts = split.decommit_unbumped_middle() + split.decommit_free_blocks();
    assert_eq!(whole, parts, "floor 0 is the two calls");
    assert_eq!(unfloored.committed_bytes(), split.committed_bytes());
    assert_eq!(unfloored.committed_bytes(), 0);
}
