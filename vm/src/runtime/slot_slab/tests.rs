use super::*;

fn fill(w: SlabWindow, n: usize, tag: u64) {
    for i in 0..n {
        // SAFETY: `w` is a live window of `n` slots.
        unsafe {
            w.vals
                .as_ptr()
                .add(i)
                .write(CompactValue::from_bits(tag + i as u64));
            w.kinds
                .as_ptr()
                .add(i)
                .write((tag as u8).wrapping_add(i as u8));
        }
    }
}

fn holds(w: SlabWindow, n: usize, tag: u64) -> bool {
    (0..n).all(|i| {
        // SAFETY: `w` is a live window of `n` slots.
        unsafe {
            w.vals.as_ptr().add(i).read().raw_bits() == tag + i as u64
                && w.kinds.as_ptr().add(i).read() == (tag as u8).wrapping_add(i as u8)
        }
    })
}

#[test]
fn a_released_window_is_the_next_one_handed_out() {
    let mut slab = SlotSlab::new();
    let a = slab.alloc(5);
    let b = slab.alloc(7);
    assert_eq!(a.mark, 0);
    assert_eq!(b.mark, 5, "windows are contiguous");
    // SAFETY: both windows are in chunk 0.
    assert_eq!(unsafe { a.vals.as_ptr().add(5) }, b.vals.as_ptr());
    slab.release_to(b.mark);
    let b2 = slab.alloc(3);
    assert_eq!(b2.vals, b.vals, "the released window is reused");
    slab.release_to(a.mark);
    assert_eq!(slab.mark(), 0);
    assert_eq!(slab.alloc(1).vals, a.vals);
}

#[test]
fn releasing_to_a_higher_or_unknown_mark_releases_nothing() {
    let mut slab = SlotSlab::new();
    let a = slab.alloc(4);
    let _b = slab.alloc(4);
    let before = slab.mark();
    slab.release_to(SLAB_MARK_UNKNOWN);
    assert_eq!(slab.mark(), before);
    slab.release_to(before + 100);
    assert_eq!(slab.mark(), before);
    slab.release_to(a.mark);
    assert_eq!(slab.mark(), a.mark);
}

#[test]
fn growth_appends_chunks_and_never_moves_a_live_window() {
    // Deep enough to cross several chunks (1024, 2048, 4096, ... slots).
    let mut slab = SlotSlab::new();
    let mut live = Vec::new();
    for depth in 0..3000u64 {
        let n = 1 + (depth as usize % 29);
        let w = slab.alloc(n);
        fill(w, n, depth * 1000);
        live.push((w, n, depth * 1000));
    }
    assert!(
        slab.chunks.len() > 2,
        "the test must cross chunk boundaries"
    );
    for &(w, n, tag) in &live {
        assert!(
            holds(w, n, tag),
            "window at mark {:#x} was disturbed",
            w.mark
        );
    }
    // Pop half the stack and push it again: the lower half is untouched and
    // at the same addresses.
    let half = live.len() / 2;
    slab.release_to(live[half].0.mark);
    for depth in 0..(live.len() - half) as u64 {
        let n = 3;
        let w = slab.alloc(n);
        fill(w, n, 7_000_000 + depth);
    }
    for &(w, n, tag) in &live[..half] {
        assert!(holds(w, n, tag), "a live window changed after a re-push");
    }
}

#[test]
fn a_window_larger_than_any_chunk_gets_one_of_its_own() {
    let mut slab = SlotSlab::new();
    let small = slab.alloc(10);
    fill(small, 10, 1);
    let n = SLAB_MAX_CHUNK + 17;
    let big = slab.alloc(n);
    fill(big, n, 50);
    assert!(holds(small, 10, 1));
    assert!(holds(big, n, 50));
    slab.release_to(big.mark);
    assert_eq!(slab.mark(), small.mark + 10);
    // A spare chunk too small for a later window is replaced, not grown in
    // place: the window below the mark is still intact.
    let again = slab.alloc(n + 1);
    fill(again, n + 1, 90);
    assert!(holds(small, 10, 1));
}

/// Stage 2 (argument overlap): a window that starts inside the top
/// window, the marks it records and restores, and every refusal.
#[test]
fn an_overlapping_window_starts_where_asked_and_its_release_restores_the_mark() {
    const SLOT: usize = std::mem::size_of::<CompactValue>();
    let mut slab = SlotSlab::new();
    assert!(
        slab.alloc_overlapping(8, 64, 1).is_none(),
        "no chunk yet: nothing to overlap"
    );
    let caller = slab.alloc(10);
    let base = caller.vals.as_ptr() as usize;
    let end = base + 10 * SLOT;
    let mark = slab.mark();
    assert_eq!(mark, 10);

    // Starts inside the caller, ends past it: the mark moves up, and the
    // window records the mark before it.
    let w = slab
        .alloc_overlapping(base + 6 * SLOT, end, 8)
        .expect("fits");
    assert_eq!(w.vals.as_ptr() as usize, base + 6 * SLOT);
    // SAFETY: both windows are in chunk 0.
    assert_eq!(w.kinds.as_ptr(), unsafe { caller.kinds.as_ptr().add(6) });
    assert_eq!(w.mark, mark);
    assert_eq!(slab.mark(), 14);

    // Nested and ending below the mark: the mark never moves down.
    let inner = slab
        .alloc_overlapping(base + 8 * SLOT, base + 14 * SLOT, 2)
        .expect("fits");
    assert_eq!(inner.mark, 14);
    assert_eq!(slab.mark(), 14);
    slab.release_to(inner.mark);
    assert_eq!(slab.mark(), 14);
    slab.release_to(w.mark);
    assert_eq!(
        slab.mark(),
        mark,
        "the callee's pop restores the caller's mark"
    );

    // Refusals, each leaving the mark alone: below the chunk; the top
    // window reaching past the mark; no free slot in the top window; not
    // slot-aligned; not fitting the chunk.
    let refused = [
        (base - SLOT, end, 1),
        (base, end + SLOT, 1),
        (end, end, 1),
        (base + 1, end, 1),
        (base, end, SLAB_FIRST_CHUNK + 1),
    ];
    for (start, top_end, n) in refused {
        assert!(slab.alloc_overlapping(start, top_end, n).is_none());
        assert_eq!(slab.mark(), mark);
    }
    // A plain window after all that lands right after the caller.
    assert_eq!(slab.alloc(1).vals.as_ptr() as usize, end);
}

#[test]
fn a_shared_window_keeps_its_size_and_an_owned_buffer_cannot_be_shared() {
    let mut slab = SlotSlab::new();
    let w = slab.alloc(6);
    // SAFETY: `w` is live for the whole test.
    let mut s = unsafe { SlotVec::window(w.kinds, 4, 6) };
    assert!(!s.is_shared_with_caller());
    s.mark_shared_with_caller();
    assert!(s.is_shared_with_caller() && s.is_window());
    assert_eq!(s.capacity(), 6);
    assert_eq!(s.len(), 4);
    assert_eq!(s.owned_capacity(), 0);
    assert!(
        s.take_vec().is_empty(),
        "a shared window is still slab memory"
    );

    let mut o = SlotVec::from_vec(vec![1u8, 2, 3]);
    o.mark_shared_with_caller();
    assert!(!o.is_shared_with_caller() && !o.is_window());
    assert_eq!(o.into_vec(), vec![1, 2, 3]);
}

#[test]
fn an_owned_slot_vec_round_trips_its_vec_with_its_capacity() {
    let mut v: Vec<u8> = Vec::with_capacity(40);
    v.extend_from_slice(&[1, 2, 3]);
    let cap = v.capacity();
    let ptr = v.as_ptr();
    let s = SlotVec::from_vec(v);
    assert!(!s.is_window());
    assert_eq!(s.len(), 3);
    assert_eq!(s.capacity(), cap);
    assert_eq!(&*s, &[1, 2, 3]);
    let back = s.into_vec();
    assert_eq!(back.as_ptr(), ptr, "no copy");
    assert_eq!(back.capacity(), cap);
    assert_eq!(back, vec![1, 2, 3]);
}

#[test]
fn a_window_hands_over_no_vec_and_copies_when_made_owned() {
    let mut slab = SlotSlab::new();
    let w = slab.alloc(4);
    fill(w, 4, 11);
    // SAFETY: `w` is live for the whole test.
    let mut s = unsafe { SlotVec::window(w.kinds, 4, 4) };
    assert!(s.is_window());
    assert_eq!(s.capacity(), 4);
    s[2] = 99;
    // SAFETY: as above.
    assert_eq!(
        unsafe { w.kinds.as_ptr().add(2).read() },
        99,
        "writes go to the slab"
    );
    let mut t = unsafe { SlotVec::<u8>::window(w.kinds, 4, 4) };
    assert!(
        t.take_vec().is_empty(),
        "a window has no allocation to hand over"
    );
    assert!(!t.is_window());
    s.make_owned();
    assert!(!s.is_window());
    assert_eq!(s[2], 99);
    s[2] = 5;
    // SAFETY: as above.
    assert_eq!(
        unsafe { w.kinds.as_ptr().add(2).read() },
        99,
        "an owned copy no longer aliases the slab"
    );
}
