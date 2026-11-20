use super::*;

#[test]
fn new_bitmap_all_clear() {
    let bm = MarkBitmap::new(0x1000, 4096);
    assert_eq!(bm.marked_count(), 0);
    assert!(!bm.is_marked(0x1000));
}

#[test]
fn try_mark_returns_true_then_false() {
    let bm = MarkBitmap::new(0x0, 1024);
    // First mark: should succeed
    assert!(bm.try_mark(0));
    assert!(bm.is_marked(0));
    // Second mark: already set
    assert!(!bm.try_mark(0));
    assert_eq!(bm.marked_count(), 1);
}

#[test]
fn mark_multiple_addresses() {
    let bm = MarkBitmap::new(0x0, 4096);
    assert!(bm.try_mark(0));
    assert!(bm.try_mark(8));
    assert!(bm.try_mark(16));
    assert!(bm.try_mark(4088)); // near end
    assert_eq!(bm.marked_count(), 4);
}

#[test]
fn out_of_range_ignored() {
    let bm = MarkBitmap::new(0x1000, 1024);
    assert!(!bm.try_mark(0x0)); // below base
    assert!(!bm.try_mark(0x2000)); // above end
    assert!(!bm.is_marked(0x0));
}

/// gc-common w3-e: the `&mut` memset clear resets exactly what `clear`
/// does -- bits, the marked-word window and the `any_marked` flag -- for a
/// windowed mark set and for one that spans the whole bitmap, and a second
/// call on an untouched bitmap is the early return.
#[test]
fn clear_exclusive_resets_like_clear() {
    let mut bm = MarkBitmap::new(0x0, 1 << 16);
    bm.try_mark(4096);
    bm.try_mark(8192 + 8);
    assert_eq!(bm.marked_count(), 2);
    bm.clear_exclusive();
    assert_eq!(bm.marked_count(), 0);
    assert!(!bm.any_marked());

    bm.try_mark(0);
    bm.try_mark((1 << 16) - 8);
    bm.clear_exclusive();
    assert_eq!(bm.marked_count(), 0);
    assert!(!bm.is_marked(0));
    assert!(!bm.is_marked((1 << 16) - 8));

    // Marks after an exclusive clear are tracked and cleared again by the
    // atomic form: the window was reset, not left stale.
    bm.try_mark(512);
    assert!(bm.any_marked());
    bm.clear();
    assert_eq!(bm.marked_count(), 0);
    bm.clear_exclusive();
}

#[test]
fn clear_resets_all() {
    let bm = MarkBitmap::new(0x0, 1024);
    bm.try_mark(0);
    bm.try_mark(64);
    bm.try_mark(512);
    assert_eq!(bm.marked_count(), 3);
    bm.clear();
    assert_eq!(bm.marked_count(), 0);
    assert!(!bm.is_marked(0));
}

/// A cleared bitmap must read as empty from another thread.
///
/// `clear()` is relaxed stores plus one trailing `Release` fence (it used
/// to be an `AcqRel` swap per word; see its doc for the measurement that
/// changed it). The property that has to hold is unchanged and is what is
/// asserted: after a clear, no reader observes a bit from before it. The
/// join below is the happens-before edge in this test, standing in for the
/// stop-the-world pause exit that provides it in production.
#[test]
fn clear_publishes_zeroes_to_marker() {
    use std::sync::Arc;
    let bm = Arc::new(MarkBitmap::new(0x0, 4096));
    // Mark many bits, then clear in a thread, and read in another —
    // every reader observation post-clear must be `false`.
    for i in 0..512 {
        bm.try_mark(i * 8);
    }
    assert!(bm.marked_count() > 0);
    let bm_clearer = bm.clone();
    let clearer = std::thread::spawn(move || {
        bm_clearer.clear();
    });
    clearer.join().unwrap();
    // After clear+join (which provides happens-before), every bit
    // must be observed as unmarked.
    for i in 0..512 {
        assert!(!bm.is_marked(i * 8), "stale mark at {i}");
    }
    assert_eq!(bm.marked_count(), 0);
}

/// `clear()` may skip its sweep ONLY for a bitmap nothing has marked into.
///
/// The direction that matters is the unsafe one: a bitmap with a bit set
/// must never take the early return, because the bit would survive into the
/// next cycle as a stale black mark and the object it names would be
/// treated as live-then-reaped. After marking finishes, both the flag and
/// the claimed bit must be present. This test does not race `clear` with
/// marking: `clear` requires exclusive access, rather than relying on the
/// flag's release store to order a later bitmap claim.
#[test]
fn clear_skips_only_an_untouched_bitmap() {
    let bm = MarkBitmap::new(0x1000, 4096);
    assert!(!bm.any_marked(), "a fresh bitmap has nothing to clear");
    bm.clear();
    assert!(!bm.any_marked());

    // A mark that LANDED arms the flag...
    assert!(bm.try_mark(0x1000));
    assert!(bm.any_marked());
    bm.clear();
    assert!(!bm.any_marked(), "clear disarms");
    assert!(!bm.is_marked(0x1000), "and actually cleared the bit");

    // ...and a re-mark of an ALREADY-set bit must keep it armed, or the
    // second clear of a cycle would skip a populated bitmap.
    assert!(bm.try_mark(0x1008));
    assert!(!bm.try_mark(0x1008), "second mark is a no-op on the bit");
    assert!(bm.any_marked(), "but must not disarm the flag");
    bm.clear();
    assert!(!bm.is_marked(0x1008));
}

/// An out-of-range mark must not arm the flag into claiming work that
/// cannot exist -- and, more importantly, must not be reported as newly
/// marked.
#[test]
fn out_of_range_mark_does_not_arm_the_flag() {
    let bm = MarkBitmap::new(0x1000, 1024);
    assert!(!bm.try_mark(0x0));
    assert!(!bm.try_mark(0x9000));
    assert!(!bm.any_marked());
}

/// The constructor hands back a genuinely zeroed bitmap.
///
/// It builds one from `vec![0u64; n]` and reinterprets the allocation as
/// `[AtomicU64]` rather than constructing each atomic; this pins the bit
/// pattern that conversion assumes.
#[test]
fn alloc_zeroed_bitmap_reads_as_empty() {
    let bm = MarkBitmap::new(0x1_0000, 1 << 20);
    assert_eq!(bm.marked_count(), 0);
    for i in (0..(1usize << 20)).step_by(4096) {
        assert!(!bm.is_marked(0x1_0000 + i), "stale bit at +{i:#x}");
    }
}

/// An address outside this bitmap's region must not be recorded ANYWHERE.
///
/// The companion to `out_of_range_ignored`, and the reason it is a separate
/// test: that one checks the return value, this one checks that nothing was
/// stored. `HeapBitmap::claim` spills an unencodable address into an
/// overflow set — correct for an object-start registry, where losing such a
/// base loses an object, and wrong for a bounded mark bitmap that G1 asks
/// about other regions' addresses as a matter of course. Without the range
/// screen in `try_mark` the overflow set would grow without bound and a
/// foreign address would be reported newly marked.
#[test]
fn an_out_of_region_mark_does_not_reach_the_overflow_set() {
    let bm = MarkBitmap::new(0x1_0000, 4096);
    assert!(!bm.try_mark(0x0), "below the region");
    assert!(!bm.try_mark(0x9_0000), "above the region");
    assert!(!bm.try_mark(0x1_0004), "inside, but not on the 8-byte grid");
    assert!(
        !bm.bits.has_spill(),
        "a mark bitmap is bounded to one region; nothing outside it may be \
         recorded, and an overflow entry is both a wrong answer and an \
         unbounded leak"
    );
    assert_eq!(bm.marked_count(), 0);
    assert!(!bm.any_marked());
}

/// gengc-round2-alloc2, 2026-09-20 — the clear is bounded to the span the
/// marks landed in, and every bit in that span still goes.
///
/// The direction that matters is the unsafe one, and it is the same one
/// `clear_skips_only_an_untouched_bitmap` guards from the other side: a
/// bit OUTSIDE the window would survive the clear as a stale black mark.
/// So the test marks at both extremes of a large bitmap — the pair that
/// makes a window computed from only one of them wrong — plus one in the
/// middle, and re-reads the whole bitmap afterwards. In a debug build
/// `clear_within`'s own `debug_assert_clear_outside` walk is the second,
/// stronger check: it asserts every word outside the bound was already
/// zero before the stores.
#[test]
fn clear_is_bounded_to_the_marked_span_and_still_clears_all_of_it() {
    const BASE: usize = 0x10_0000;
    const SPAN: usize = 1 << 20; // 2048 bitmap words
    let bm = MarkBitmap::new(BASE, SPAN);
    assert!(bm.bits.word_count() > 64, "need a bitmap worth bounding");

    // First and last grid slots, and one in between: a window derived from
    // any single mark would leave one of the other two behind.
    let marks = [BASE, BASE + SPAN / 2, BASE + SPAN - 8];
    for m in marks {
        assert!(bm.try_mark(m), "{m:#x} must be in range and newly marked");
    }
    assert_eq!(bm.marked_count(), 3);
    bm.clear();
    for m in marks {
        assert!(!bm.is_marked(m), "stale mark survived at {m:#x}");
    }
    assert_eq!(bm.marked_count(), 0);
    assert!(!bm.any_marked());

    // The window must RESET with the clear, or the next cycle's bound
    // would be the union of every cycle so far and grow back to the
    // capacity it exists to avoid. It is also where the NARROWING is
    // observable: one word out of 2048, against a `clear_all` that would
    // store into all of them.
    assert_eq!(bm.mark_lo_word.load(Ordering::Relaxed), usize::MAX);
    assert_eq!(bm.mark_hi_word.load(Ordering::Relaxed), 0);
    assert!(bm.try_mark(BASE + 512));
    let (lo, hi) = (
        bm.mark_lo_word.load(Ordering::Relaxed),
        bm.mark_hi_word.load(Ordering::Relaxed),
    );
    // One bitmap word covers 512 ARENA bytes (64 bits x 8), so offset 512
    // is bit 64, i.e. word 1 — and the window is half-open.
    assert_eq!((lo, hi), (1, 2));
    assert!(hi - lo < bm.bits.word_count() / 64, "the bound must bite");
    bm.clear();
    assert!(!bm.is_marked(BASE + 512));
}

/// A mark the range screen rejects must not widen the window either.
///
/// The companion to `out_of_range_mark_does_not_arm_the_flag`: an
/// out-of-region address is one G1 asks every region's bitmap about as a
/// matter of course, and letting one widen the window would restore the
/// full-capacity clear on the first foreign probe of a cycle.
#[test]
fn an_out_of_range_mark_does_not_widen_the_clear_window() {
    let bm = MarkBitmap::new(0x1000, 4096);
    assert!(!bm.try_mark(0x0));
    assert!(!bm.try_mark(0x9000));
    assert!(!bm.try_mark(0x1004), "inside, but off the 8-byte grid");
    assert_eq!(bm.mark_lo_word.load(Ordering::Relaxed), usize::MAX);
    assert_eq!(bm.mark_hi_word.load(Ordering::Relaxed), 0);
}

/// A RE-mark of an already-set bit must keep the window as wide as the
/// first one made it.
///
/// `try_mark` returns `false` for a bit that is already set, and a window
/// widened only on the newly-set path would be correct only because the
/// first mark already widened it. Widening before the claim — on every
/// in-range call — is what makes that independent of `claim`'s answer, and
/// this is the test that says so.
#[test]
fn a_repeat_mark_cannot_narrow_the_window() {
    let bm = MarkBitmap::new(0x0, 1 << 16);
    assert!(bm.try_mark(0x8000));
    let before = (
        bm.mark_lo_word.load(Ordering::Relaxed),
        bm.mark_hi_word.load(Ordering::Relaxed),
    );
    assert!(!bm.try_mark(0x8000), "second mark is a no-op on the bit");
    assert_eq!(
        (
            bm.mark_lo_word.load(Ordering::Relaxed),
            bm.mark_hi_word.load(Ordering::Relaxed),
        ),
        before,
    );
    bm.clear();
    assert!(!bm.is_marked(0x8000));
}

#[test]
fn concurrent_marking() {
    use std::sync::Arc;
    let bm = Arc::new(MarkBitmap::new(0x0, 8192));
    let mut handles = Vec::new();

    for t in 0..4 {
        let bm = bm.clone();
        handles.push(std::thread::spawn(move || {
            let mut newly_marked = 0usize;
            for i in 0..256 {
                let addr = ((t * 256 + i) * MARK_GRANULARITY) % 8192;
                if bm.try_mark(addr) {
                    newly_marked += 1;
                }
            }
            newly_marked
        }));
    }

    let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    // Each address should be marked exactly once (no overlap in ranges)
    assert_eq!(total, bm.marked_count());
}
