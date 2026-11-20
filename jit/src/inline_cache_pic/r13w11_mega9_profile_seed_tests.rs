//! Round 13 wave 11, lane mega9, proposal M13-9: a slot whose site's
//! receiver profile already shows a megamorphic site is born flagged, so
//! its gate sends receivers to the cell-first stub from the first call.
use super::*;

fn counts(rows: &[(u32, u32)]) -> crate::profile::ReceiverCounts {
    rows.iter().copied().collect()
}

/// Raw, even entry words (bit 0 is the context tag): published, never called.
fn word(class_id: u32) -> u64 {
    0x1000 + (u64::from(class_id) << 4)
}

#[test]
fn only_a_profile_past_the_inline_ways_with_a_real_overflow_names_a_megamorphic_site() {
    fn names(rows: &[(u32, u32)]) -> bool {
        profile_names_megamorphic_site(&counts(rows))
    }
    // Four classes fit the inline ways, however many observations.
    assert!(!names(&[(1, 100), (2, 100), (3, 100), (4, 100)]));
    // Eight uniform classes: half the calls overflow the top four.
    let uniform: Vec<(u32, u32)> = (1..=8).map(|c| (c, 10)).collect();
    assert!(names(&uniform));
    // A fifth class seen once in a hundred is a trace, not a site shape.
    assert!(!names(&[(1, 30), (2, 30), (3, 20), (4, 19), (5, 1)]));
    // Exactly a sixteenth overflowing counts.
    assert!(names(&[(1, 4), (2, 4), (3, 4), (4, 3), (5, 1)]));
    // Too few observations say nothing.
    assert!(!names(&[(1, 2), (2, 2), (3, 2), (4, 2), (5, 2)]));
    // A zero row is not a class seen.
    assert!(!names(&[(1, 10), (2, 10), (3, 10), (4, 10), (5, 0)]));
    assert!(!names(&[]));
}

#[test]
fn a_seeded_slot_is_megamorphic_from_birth_and_publishes_hashed_first() {
    let uniform: Vec<(u32, u32)> = (1..=8).map(|c| (c, 10)).collect();
    let pic = JitPICSlot::new();
    pic.note_inline_ways_probed();
    assert!(pic.inline_first());
    assert!(pic.seed_megamorphic_from_profile(&counts(&uniform)));
    assert!(pic.is_megamorphic());
    assert!(!pic.inline_first(), "a megamorphic site is hashed-first");
    pic.install(3, "C", word(3), false, false);
    assert!(
        pic.lookup_megamorphic(3).is_some(),
        "the receiver takes a hashed way, where the cell-first stub reads it"
    );
    pic.clear_entries();
    // A thin profile seeds nothing.
    let thin = JitPICSlot::new();
    assert!(!thin.seed_megamorphic_from_profile(&counts(&[(1, 50), (2, 50)])));
    assert!(!thin.is_megamorphic());    }