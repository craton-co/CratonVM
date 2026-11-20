use super::*;

/// The in-place aged word is `set_gc_age(gc_age() + step)`'s word for every
/// age, flag mix and step, saturating at `MAX_GC_AGE`, keeps every flag
/// (unlike the sweep's word, which clears `GC_FLAG_MARKED`), and step 0 is
/// the identity.
#[test]
fn t1w8_pin10_in_place_aged_mark_is_the_set_gc_age_word() {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 256 * 1024);
    let o = heap.alloc_object(ClassId::new(3), 1);
    let h = heap.get_header(o);
    let original = h.mark_word.load(Ordering::Relaxed);
    for step in [0u8, 1, 2, 15, 255] {
        for age in 0..=cratonvm_types::MAX_GC_AGE {
            for flags in [0u8, GC_FLAG_MARKED, GC_FLAG_OLD_GEN] {
                h.mark_word.store(original, Ordering::Relaxed);
                h.set_gc_flags(flags);
                h.set_gc_age(age);
                let m = h.mark_word.load(Ordering::Relaxed);
                h.set_gc_age(h.gc_age().saturating_add(step));
                let want = h.mark_word.load(Ordering::Relaxed);
                let (word, new_age) = in_place_aged_mark(m, step);
                assert_eq!(word, want, "step {step} age {age} flags {flags:#x}");
                assert_eq!(new_age, h.gc_age(), "step {step} age {age} flags {flags:#x}");
                assert_eq!(ObjectHeader::gc_flags_of(word), flags, "flags kept");
                if step == 0 {
                    assert_eq!(word, m, "step 0 is the identity");
                }
            }
        }
    }
    h.mark_word.store(original, Ordering::Relaxed);
}

/// `age_in_place_survivor` stores exactly `in_place_aged_mark`'s word.
#[test]
fn t1w8_pin10_age_in_place_survivor_stores_the_aged_word() {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 256 * 1024);
    let o = heap.alloc_object(ClassId::new(3), 1);
    let h = heap.get_header(o);
    let original = h.mark_word.load(Ordering::Relaxed);
    h.set_gc_age(2);
    let before = h.mark_word.load(Ordering::Relaxed);
    // SAFETY: a live, unforwarded object of this single-threaded test heap.
    let ages = unsafe { age_in_place_survivor(o.as_ptr() as usize, 1) };
    assert_eq!(ages, (2, 3));
    assert_eq!(h.mark_word.load(Ordering::Relaxed), in_place_aged_mark(before, 1).0);
    h.mark_word.store(original, Ordering::Relaxed);
}

