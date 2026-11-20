//! Round 13 wave 8, lane mega7
//! (`r13w8-mega7-site-caches-keep-unloaded-receivers-FIXED-20260928.md`): a class
//! unload empties a MIC and retires the PIC ways keyed on the unloaded
//! receiver, whatever their target; live receivers keep theirs.
use super::*;
use std::sync::atomic::Ordering;

/// Raw, even entry words (bit 0 is the context tag): published, never called.
fn word(class_id: u32) -> u64 {
    0x2000 + (u64::from(class_id) << 4)
}

#[test]
fn an_unloaded_receiver_leaves_the_site_caches() {
    let pic = JitPICSlot::new();
    // An unmarked site: every receiver takes a hashed way and an inline way.
    for c in 1..=4 {
        pic.install(c, "C", word(c), false, false);
    }
    let dead = |c: u32| c == 2 || c == 3;
    let hashed_dead = [2u32, 3]
        .iter()
        .filter(|&&c| pic.lookup_megamorphic(c).is_some())
        .count();
    assert_eq!(
        pic.retire_receiver_classes(&|c| c == 99),
        0,
        "nothing to retire"
    );
    assert_eq!(pic.retire_receiver_classes(&dead), 2 + hashed_dead);
    for c in [2u32, 3] {
        assert_eq!(pic.lookup(c), None, "class {c}'s inline way");
        assert_eq!(pic.lookup_megamorphic(c), None, "class {c}'s hashed way");
    }
    for c in [1u32, 4] {
        assert_eq!(pic.lookup(c), Some((word(c), false)), "live class {c}");
    }
    assert_eq!(pic.entries_used(), 2);
    assert_eq!(pic.retire_receiver_classes(&dead), 0, "already retired");
    pic.clear_entries();

    let mic = JitMICSlot::new();
    mic.update(5, "C", word(5), false, false);
    assert_eq!(mic.cached_entry(), (word(5), false));
    assert!(
        !mic.forget_dead_receiver(&|c| c == 6),
        "a live receiver stays"
    );
    assert!(mic.forget_dead_receiver(&|c| c == 5));
    assert_eq!(
        mic.cached_class_id.load(Ordering::Acquire),
        JitMICSlot::EMPTY_CLASS_ID
    );
    assert_eq!(mic.cached_entry(), (0, false));
    assert!(!mic.forget_dead_receiver(&|c| c == 5), "already empty");
    // Empty again, so the next receiver class gets the monomorphic path
    // back; before, a MIC filled by an unloaded class served nobody again.
    mic.update(8, "D", word(8), true, false);
    assert_eq!(mic.cached_class_id.load(Ordering::Acquire), 8);
    assert_eq!(mic.cached_entry(), (word(8), true));
    mic.clear_compiled_entry();
}
