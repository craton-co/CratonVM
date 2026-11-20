use super::*;
use std::sync::atomic::Ordering;

/// One artifact carrying one MIC slot in a chosen state.
///
/// `entry` defaults non-zero in every helper below that means "installed",
/// because a zero entry is `prepopulate`'s guard HINT rather than an
/// observation — see the accessor's doc comment.
fn artifact_with(bci: usize, class_id: u32, entry: u64, misses: u64) -> CompiledMethod {
    let mut cm = CompiledMethod::new(ExecutableBuffer::new(64).unwrap());
    let slot = Box::new(JitMICSlot::new_at(bci));
    slot.cached_class_id.store(class_id, Ordering::Relaxed);
    slot.cached_entry_word.store(entry, Ordering::Relaxed);
    slot.misses.store(misses, Ordering::Relaxed);
    cm._jit_mic_slots.push(slot);
    cm
}

const ENTRY: u64 = 0x4000;

/// The shape that matters, and the one a statistical bar would have thrown
/// away: installed once, then served from machine code forever, so `hits`
/// is 0 and `misses` is 1. This is what `objectsAreEqual` actually looks
/// like — measured, `bci16:cls1167:h0:m1`.
#[test]
fn an_installed_slot_is_evidence_even_with_zero_recorded_hits() {
    let cm = artifact_with(16, 77, ENTRY, 1);
    assert_eq!(cm._jit_mic_slots[0].hits.load(Ordering::Relaxed), 0);
    assert_eq!(cm.dominant_receiver_at_bci(16), Some(77));
}

/// JIT round 19 wave 3 (lane splice3, SPLICE2-3): a bci the artifact
/// guarded-spliced keeps its call (and this slot) on the class test's
/// MISS edge, so the installed class is one the test rejected -- the
/// minority receiver. The slot answers for no other bci than its own.
#[test]
fn a_guarded_spliced_bci_s_miss_edge_cache_is_no_evidence() {
    let mut cm = artifact_with(16, 77, ENTRY, 1);
    cm.ir_guarded_splice_bcis = vec![16].into_boxed_slice();
    assert_eq!(cm.dominant_receiver_at_bci(16), None);
    let mut other = artifact_with(16, 77, ENTRY, 1);
    other.ir_guarded_splice_bcis = vec![3, 20].into_boxed_slice();
    assert_eq!(other.dominant_receiver_at_bci(16), Some(77));
}

/// JIT round 19 wave 4 (lane splice4, SPLICE3-1): the guard class a
/// guarded-spliced bci TESTED answers for that bci only, and a class `0`
/// row answers nothing.
#[test]
fn a_guarded_spliced_bci_answers_its_guard_class() {
    let mut cm = artifact_with(16, 77, ENTRY, 1);
    cm.ir_guarded_splice_bcis = vec![3, 16].into_boxed_slice();
    cm.ir_guarded_splice_classes = vec![(3, 0), (16, 91), (20, 5)].into_boxed_slice();
    assert_eq!(cm.dominant_receiver_at_bci(16), None, "the miss-edge cache");
    assert_eq!(cm.guarded_splice_class_at_bci(16), Some(91));
    assert_eq!(cm.guarded_splice_class_at_bci(20), Some(5));
    assert_eq!(cm.guarded_splice_class_at_bci(3), None);
    assert_eq!(cm.guarded_splice_class_at_bci(17), None);
    assert_eq!(cm.guarded_splice_class_at_bci(usize::MAX), None);
    assert_eq!(
        artifact_with(16, 77, ENTRY, 1).guarded_splice_class_at_bci(16),
        None
    );
}

/// The bci is the whole point. Before `JitMICSlot::bci` existed the
/// artifact kept the boxes and threw the pc mapping away, so there was no
/// way to ask this question at all — every slot looked like every other.
#[test]
fn a_slot_for_another_bci_does_not_answer_for_this_one() {
    let cm = artifact_with(16, 77, ENTRY, 1);
    assert_eq!(cm.dominant_receiver_at_bci(20), None);
}

/// A class id with no installed entry is `prepopulate`'s seed from PROFILE
/// data — a guard hint, not an observation. Accepting it would launder a
/// profile guess back in dressed as runtime evidence, which is exactly the
/// thing this accessor exists to substitute for.
#[test]
fn a_seeded_class_with_no_installed_entry_is_not_evidence() {
    let cm = artifact_with(16, 77, 0, 0);
    assert_eq!(cm.dominant_receiver_at_bci(16), None);
    // The same slot, once something is actually installed, answers.
    let cm = artifact_with(16, 77, ENTRY, 0);
    assert_eq!(cm.dominant_receiver_at_bci(16), Some(77));
}

/// Misses past the PIC-promotion threshold mean the site has seen
/// receivers this slot could not serve. That is the one thing these
/// counters measure honestly — the helper is entered on every one.
#[test]
fn misses_past_the_pic_promotion_threshold_are_not_evidence() {
    let cm = artifact_with(16, 77, ENTRY, MIC_TO_PIC_THRESHOLD);
    assert_eq!(cm.dominant_receiver_at_bci(16), Some(77));
    let cm = artifact_with(16, 77, ENTRY, MIC_TO_PIC_THRESHOLD + 1);
    assert_eq!(cm.dominant_receiver_at_bci(16), None);
}

/// A PIC that has taken a miss at this bci means the adaptive recompiler
/// already concluded the site is polymorphic. The MIC beside it still holds
/// whichever receiver it installed FIRST, which is the least informative of
/// several — guarding on it would be guarding on an accident of ordering.
#[test]
fn a_polymorphic_site_is_not_evidence_even_though_its_mic_is_populated() {
    let mut cm = artifact_with(16, 77, ENTRY, 1);
    assert_eq!(cm.dominant_receiver_at_bci(16), Some(77));
    let pic = Box::new(JitPICSlot::new_at(16));
    pic.misses.store(5, Ordering::Relaxed);
    cm._jit_pic_slots.push(pic);
    assert_eq!(cm.dominant_receiver_at_bci(16), None);
}

/// ...but a PIC at a DIFFERENT bci says nothing about this one, and an
/// unused PIC (no misses) is not a polymorphism verdict either.
#[test]
fn an_unrelated_or_unused_pic_does_not_veto() {
    let mut cm = artifact_with(16, 77, ENTRY, 1);
    let elsewhere = Box::new(JitPICSlot::new_at(99));
    elsewhere.misses.store(5, Ordering::Relaxed);
    cm._jit_pic_slots.push(elsewhere);
    assert_eq!(cm.dominant_receiver_at_bci(16), Some(77));
    cm._jit_pic_slots.push(Box::new(JitPICSlot::new_at(16)));
    assert_eq!(cm.dominant_receiver_at_bci(16), Some(77));
}

/// r11-tier-pic-miss-counter-never-moves: production never moves a PIC's
/// `misses` (only the diagnostic `lookup` does), so the classes the PIC
/// PUBLISHED are the polymorphism evidence. Two distinct live classes, or
/// one the MIC does not hold, veto; the MIC's own class alone does not.
#[test]
fn a_pic_that_published_a_second_class_vetoes_without_any_miss() {
    let mut cm = artifact_with(16, 77, ENTRY, 1);
    let pic = Box::new(JitPICSlot::new_at(16));
    pic.class_ids[0].store(77, Ordering::Release);
    cm._jit_pic_slots.push(pic);
    assert_eq!(
        cm.dominant_receiver_at_bci(16),
        Some(77),
        "the MIC's class alone"
    );
    cm._jit_pic_slots[0].class_ids[1].store(88, Ordering::Release);
    assert_eq!(cm.dominant_receiver_at_bci(16), None, "two live ways");

    let mut cm = artifact_with(16, 77, ENTRY, 1);
    let pic = Box::new(JitPICSlot::new_at(16));
    pic.class_ids[0].store(88, Ordering::Release);
    cm._jit_pic_slots.push(pic);
    assert_eq!(
        cm.dominant_receiver_at_bci(16),
        None,
        "a class the MIC lacks"
    );

    let mut cm = artifact_with(16, 77, ENTRY, 1);
    let pic = Box::new(JitPICSlot::new_at(16));
    pic.mega_class_ids[3].store(88, Ordering::Release);
    cm._jit_pic_slots.push(pic);
    assert_eq!(
        cm.dominant_receiver_at_bci(16),
        None,
        "the hashed table counts too"
    );
}

/// An unpopulated slot, and one caught mid-publication, are both "no
/// answer" rather than class 0 / class u32::MAX. The installing sentinel is
/// the one a concurrent reader can actually observe.
#[test]
fn an_empty_or_installing_slot_is_not_evidence() {
    let cm = artifact_with(16, 0, ENTRY, 0);
    assert_eq!(cm.dominant_receiver_at_bci(16), None);
    let cm = artifact_with(16, JitMICSlot::EMPTY_CLASS_ID, ENTRY, 0);
    assert_eq!(cm.dominant_receiver_at_bci(16), None);
    let cm = artifact_with(16, JitMICSlot::INSTALLING_CLASS_ID, ENTRY, 0);
    assert_eq!(cm.dominant_receiver_at_bci(16), None);
}

/// A slot with no site — the loop-unroll clones and every test helper —
/// must not answer for bci `usize::MAX` or for anything else.
#[test]
fn a_siteless_slot_answers_for_nothing() {
    let mut cm = CompiledMethod::new(ExecutableBuffer::new(64).unwrap());
    let slot = Box::new(JitMICSlot::new());
    slot.cached_class_id.store(77, Ordering::Relaxed);
    slot.cached_entry_word.store(ENTRY, Ordering::Relaxed);
    cm._jit_mic_slots.push(slot);
    assert_eq!(cm.dominant_receiver_at_bci(0), None);
    assert_eq!(cm.dominant_receiver_at_bci(16), None);
}
