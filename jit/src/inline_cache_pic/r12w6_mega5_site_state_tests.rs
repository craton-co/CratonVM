//! Round 12 wave 6, lane mega5: the megamorphic flag the gate reads (W5-4)
//! and the inline-first publication (W5-6). The machine-code side is
//! executed in `runtime_lowering::r12w6_mega5_cell_first_tests`.
use super::*;
use std::sync::atomic::Ordering;

/// Raw, even entry words (bit 0 is the context tag): published, never called.
fn word(class_id: u32) -> u64 {
    0x1000 + (u64::from(class_id) << 4)
}

fn hashed_holds(pic: &JitPICSlot, class_id: u32) -> bool {
    pic.lookup_megamorphic(class_id).is_some()
}

#[test]
fn the_flag_sits_where_the_gate_reads_it_and_nothing_else_moved() {
    let pic = JitPICSlot::new();
    let base = &pic as *const JitPICSlot as usize;
    assert_eq!(
        &pic.megamorphic as *const _ as usize - base,
        JitPICSlot::MEGAMORPHIC_OFFSET
    );
    assert_eq!(&pic.site_bits as *const _ as usize - base, 52);
    assert_eq!(&pic.hits as *const _ as usize - base, 56);
    assert_eq!(&pic.misses as *const _ as usize - base, 88);
    assert_eq!(
        &pic.mega_class_ids as *const _ as usize - base,
        JitPICSlot::MEGA_CLASS_IDS_OFFSET
    );
    assert_eq!(pic.megamorphic.load(Ordering::Relaxed), 0);
    assert_eq!(pic.site_bits.load(Ordering::Relaxed), 0);
}

#[test]
fn five_live_classes_make_a_site_megamorphic_and_nothing_less_does() {
    let pic = JitPICSlot::new();
    for c in 1..=4 {
        pic.install(c, "C", word(c), false, false);
    }
    pic.install(1, "C", word(1), false, false);
    assert!(!pic.is_megamorphic(), "four classes fit");
    // A receiver whose callee recompiled retires its way first; the
    // retired way is not a live fifth class.
    pic.install(2, "C", word(2) + 0x100_0000, false, false);
    assert!(!pic.is_megamorphic());
    // An unpublishable entry (zero) never counts.
    pic.install(9, "C", 0, false, false);
    assert!(!pic.is_megamorphic());
    let full = JitPICSlot::new();
    for c in 1..=5 {
        full.install(c, "C", word(c), false, false);
    }
    assert!(full.is_megamorphic());
    full.clear_entries();
    assert!(full.is_megamorphic(), "sticky across a clear");
    pic.clear_entries();
}

#[test]
fn an_unmarked_site_still_duplicates_its_inline_receivers() {
    let pic = JitPICSlot::new();
    pic.install(3, "C", word(3), false, false);
    assert_eq!(pic.entries_used(), 1);
    assert!(hashed_holds(&pic, 3), "the historical hashed-first order");
    pic.clear_entries();
}




/// t1refl: the planners that mark a slot bound never ask the switch (the
/// mark stays whatever it says); only the publication path does.
#[test]
fn t1refl_the_hashed_ways_switch_is_read_on_the_publication_path_only() {
    let src = include_str!("../inline_cache_pic.rs").replace("\r\n", "\n");
    let install = src.find("fn install_once(").expect("the publication path");
    let window: String = src[install..].chars().take(4000).collect();
    assert!(window.contains("self.is_bound_site() && !bound_pic_hashed_ways_enabled()"));
    let overflow = src
        .find("fn note_overflow_locked(")
        .expect("the overflow rule");
    let window: String = src[overflow..].chars().take(900).collect();
    assert!(window.contains("self.is_bound_site() && !bound_pic_hashed_ways_enabled()"));
    for planner in [include_str!("../lib.rs"), include_str!("../x64/op_control.rs")] {
        assert!(!planner.contains("bound_pic_hashed_ways_enabled"));
    }
}

#[test]
fn a_marked_site_publishes_inline_first_until_it_overflows() {
    let pic = JitPICSlot::new();
    pic.note_inline_ways_probed();
    assert!(pic.inline_first());
    for c in 1..=4 {
        pic.install(c, "C", word(c), false, false);
        assert!(!hashed_holds(&pic, c), "class {c} took an inline way only");
    }
    assert_eq!(pic.entries_used(), 4);
    // The overflow: the fifth class goes to the hashed set, the site turns
    // megamorphic, and every live inline receiver gains a hashed way (a
    // full set aside) with its own word.
    pic.install(5, "C", word(5), false, false);
    assert!(pic.is_megamorphic());
    assert!(
        !pic.inline_first(),
        "a megamorphic site is hashed-first again"
    );
    for i in 0..JIT_PIC_ENTRIES {
        let (class_id, entry, _) = pic.way(i).expect("a way");
        if JitPICSlot::is_live_class_id(class_id) && entry != 0 {
            let base = JitPICSlot::mega_base_index(class_id);
            let set_full_of_others = (base..base + JIT_MEGA_WAYS).all(|index| {
                let other = pic.mega_class_ids[index].load(Ordering::Acquire);
                other != class_id && JitPICSlot::is_live_class_id(other)
            });
            assert!(
                set_full_of_others || pic.lookup_megamorphic(class_id) == Some((entry, false)),
                "inline receiver {class_id} was not promoted"
            );
        }
    }
    assert_eq!(pic.lookup_megamorphic(5), Some((word(5), false)));
    pic.clear_entries();

    // A recompiled callee at a full marked site: the old way retires (it
    // is skipped as a candidate in the same pass), so the inline ways
    // refuse the new word and the hashed set takes it; a retired way is
    // not a fifth live class.
    let site = JitPICSlot::new();
    site.note_inline_ways_probed();
    for c in 1..=4 {
        site.install(c, "C", word(c), false, false);
    }
    site.install(1, "C", word(1) + 0x100_0000, false, false);
    assert!(!site.is_megamorphic());
    assert_eq!(
        site.lookup_megamorphic(1),
        Some((word(1) + 0x100_0000, false))
    );
    site.clear_entries();
}

/// Inline-first leaves no stale hashed way behind: a hashed way that
/// still names a receiver with an older target is retired when the
/// receiver's new target is published inline.
#[test]
fn an_inline_first_publication_retires_a_stale_hashed_way() {
    let pic = JitPICSlot::new();
    // Published before the mark: hashed and inline both hold class 6.
    pic.install(6, "C", word(6), false, false);
    assert!(hashed_holds(&pic, 6));
    pic.note_inline_ways_probed();
    // Its callee recompiled: the inline way retires, a free way takes the
    // new target, and the hashed way with the old one goes.
    pic.install(6, "C", word(6) + 0x100_0000, false, false);
    assert_ne!(
        pic.lookup_megamorphic(6),
        Some((word(6), false)),
        "stale way kept"
    );
    pic.clear_entries();
}
