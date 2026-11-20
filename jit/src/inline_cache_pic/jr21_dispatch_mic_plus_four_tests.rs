use super::*;

fn counts(rows: &[(u32, u32)]) -> crate::profile::ReceiverCounts {
    rows.iter().copied().collect()
}

#[test]
fn a_mic_first_slot_counts_five_inline_classes() {
    let five: Vec<(u32, u32)> = (1..=5).map(|c| (c, 20)).collect();
    let six: Vec<(u32, u32)> = (1..=6).map(|c| (c, 20)).collect();
    let plain = JitPICSlot::new();
    assert!(!plain.mic_read_first());
    assert!(plain.seed_megamorphic_from_profile(&counts(&five)));
    let first = JitPICSlot::new();
    first.note_mic_read_first();
    assert!(first.mic_read_first());
    assert!(!first.seed_megamorphic_from_profile(&counts(&five)));
    assert!(!first.is_megamorphic());
    assert!(first.seed_megamorphic_from_profile(&counts(&six)));
    assert!(profile_names_megamorphic_site(&counts(&five)));
    assert!(!profile_names_megamorphic_site_over(
        &counts(&five),
        JIT_PIC_ENTRIES + 1
    ));
}

#[test]
fn a_mic_first_slot_seeds_nothing_from_its_mic_and_names_the_mic_class() {
    use std::sync::atomic::Ordering;
    let mic = JitMICSlot::new();
    mic.prepopulate(7);
    let first = JitPICSlot::new();
    first.note_mic_read_first();
    first.seed_from_mic(&mic, false);
    for i in 0..JIT_PIC_ENTRIES {
        assert_ne!(
            first.class_ids[i].load(Ordering::Acquire),
            7,
            "way {i} took the MIC's class"
        );
    }
    // A class-only MIC (entry 0) does not serve the class inline yet.
    assert!(!first.mic_serves_class_inline(&mic, 7));
    mic.cached_entry_word.store(0x2000, Ordering::Release);
    assert!(first.mic_serves_class_inline(&mic, 7));
    assert!(!first.mic_serves_class_inline(&mic, 8));
    let plain = JitPICSlot::new();
    assert!(
        !plain.mic_serves_class_inline(&mic, 7),
        "not a MIC-first slot"
    );
    // Never a bound site: its ways are its only cache.
    let bound = JitPICSlot::new();
    bound.note_mic_read_first();
    bound.mark_bound_site();
    assert!(!bound.mic_serves_class_inline(&mic, 7));
    // t1 w7 NBD: never once megamorphic (a gate-first site's gate skips
    // the MIC; the class needs its hashed way).
    let mega = JitPICSlot::new();
    mega.note_mic_read_first();
    mega.megamorphic.store(1, Ordering::Release);
    assert!(!mega.mic_serves_class_inline(&mic, 7));
    // The raw word is never called; clear it before the slot drops.
    mic.cached_entry_word.store(0, Ordering::Release);
}
