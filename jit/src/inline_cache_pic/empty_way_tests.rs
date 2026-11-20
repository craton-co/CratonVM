//! A never-published way must not be matchable by ANY receiver, including
//! a `ClassId(0)` one — see the `class_ids` field doc and
//! `docs/feature-designs/jit-ir-optimizations.md` (F1).
use super::*;
use std::sync::atomic::Ordering;

#[test]
fn a_fresh_slot_has_no_way_a_class_zero_receiver_can_match() {
    let pic = JitPICSlot::new();
    for i in 0..JIT_PIC_ENTRIES {
        let cid = pic.class_ids[i].load(Ordering::Acquire);
        assert_ne!(cid, 0, "fresh way {i} would match a ClassId(0) receiver");
        assert!(
            !JitPICSlot::is_live_class_id(cid),
            "fresh way {i} reads live"
        );
    }
    for i in 0..JIT_MEGA_ENTRIES {
        assert_ne!(
            pic.mega_class_ids[i].load(Ordering::Acquire),
            0,
            "fresh hashed-table way {i} would match a ClassId(0) receiver"
        );
    }
    assert_eq!(pic.entries_used(), 0);
    assert!(pic.lookup(0).is_none());
    assert!(pic.lookup_megamorphic(0).is_none());
}

#[test]
fn a_fresh_way_is_still_free_to_publish_into() {
    let pic = JitPICSlot::new();
    pic.install(5, "Five", 0x5000, false, false);
    assert_eq!(pic.lookup(5), Some((0x5000, false)));
    assert_eq!(pic.lookup_megamorphic(5), Some((0x5000, false)));
    assert_eq!(pic.entries_used(), 1);
    // Every way the install did not use is still unmatchable by class 0.
    for i in 0..JIT_PIC_ENTRIES {
        let cid = pic.class_ids[i].load(Ordering::Acquire);
        if cid != 5 {
            assert_ne!(cid, 0, "unused way {i} reads 0 after an install");
        }
    }
    // All four ways fill, exactly as before the sentinel change.
    pic.install(6, "Six", 0x6000, false, false);
    pic.install(7, "Seven", 0x7000, true, false);
    pic.install(8, "Eight", 0x8000, false, false);
    assert_eq!(pic.entries_used(), JIT_PIC_ENTRIES);
}

#[test]
fn seeding_from_a_mic_still_lands_in_way_zero() {
    let mic = JitMICSlot::new();
    mic.update(7, "Seven", 0x7000, true, false);
    let pic = JitPICSlot::new();
    pic.seed_from_mic(&mic, false);
    assert_eq!(pic.class_ids[0].load(Ordering::Acquire), 7);
    assert_eq!(pic.lookup(7), Some((0x7000, true)));
}

/// A way whose guard matches but whose word is zero — a profile seed the
/// helper has not filled in yet, or a way retired between the two stores —
/// is what generated code sends to the resolving helper. The Rust mirror
/// must answer the same, and must not charge that way a hit.
#[test]
fn a_class_only_way_is_a_miss_not_a_zero_target() {
    let pic = JitPICSlot::new();
    // Publish way 0 exactly as a profile seed does: the class id with no
    // target behind it.
    pic.entry_words[0].store(0, Ordering::SeqCst);
    pic.class_ids[0].store(7, Ordering::Release);
    assert_eq!(pic.entries_used(), 1, "the way holds a live receiver");
    assert!(
        pic.lookup(7).is_none(),
        "a zero word must miss, not dispatch to address 0"
    );
    assert_eq!(pic.hits[0].load(Ordering::Relaxed), 0);
    assert_eq!(pic.misses.load(Ordering::Relaxed), 1);

    // A LATER way holding a real target for the same receiver still wins:
    // the cascade keeps comparing after a zero word takes its miss edge.
    pic.entry_words[1].store(jit_ic_entry_word(0x7000, true), Ordering::SeqCst);
    pic.class_ids[1].store(7, Ordering::Release);
    assert_eq!(pic.lookup(7), Some((0x7000, true)));
    assert_eq!(
        pic.hits[0].load(Ordering::Relaxed),
        0,
        "way 0 served nothing"
    );
    assert_eq!(pic.hits[1].load(Ordering::Relaxed), 1);
}

/// Round 9 wave 2: the MIC's empty sentinel is not a class, so seeding a
/// PIC from a never-filled MIC must copy nothing into way 0.
#[test]
fn the_mic_empty_sentinel_is_not_a_live_class_id() {
    assert!(!JitPICSlot::is_live_class_id(JitMICSlot::EMPTY_CLASS_ID));
    assert!(!JitPICSlot::is_live_class_id(0));
    assert!(JitPICSlot::is_live_class_id(7));
    let mic = JitMICSlot::new();
    let pic = JitPICSlot::new();
    pic.seed_from_mic(&mic, false);
    assert_eq!(pic.entries_used(), 0);
    assert!(pic.lookup(JitMICSlot::EMPTY_CLASS_ID).is_none());
}
