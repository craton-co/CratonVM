use super::near_globals::{in_reach, nth_hint, walk, CursorUpdate, Hint, WINDOW};

#[test]
#[cfg(target_pointer_width = "64")]
fn small_buffers_pack_pages_without_exhausting_a_free_near_band() {
    let anchor = 0x2000_0000_0000usize;
    let low = anchor - (1 << 30);
    let high = anchor - (1 << 20);
    let page = super::code_page_size();
    let occupied = std::cell::RefCell::new(Vec::<(usize, usize)>::new());
    let mut cursor = low;
    for index in 0..768 {
        let size = [14240usize, 19104, 12512, 65536, 4097][index % 5];
        let rounded = (size + page - 1) & !(page - 1);
        let placed = walk(
            size,
            cursor,
            anchor,
            |hint| {
                let start = hint as usize;
                let end = start.saturating_add(rounded);
                let mut spans = occupied.borrow_mut();
                if start >= low
                    && end <= high
                    && spans.iter().all(|&(lo, hi)| end <= lo || start >= hi)
                {
                    spans.push((start, end));
                    Some(hint)
                } else {
                    // The OS ignores an occupied/unavailable hint and maps far
                    // away. That mapping is released by the ordinary walker.
                    Some((anchor + 2 * WINDOW) as *mut u8)
                }
            },
            |_| {},
        );
        assert!(
            placed.placed.is_some() && !placed.retire,
            "a free 1GiB near band must hold small buffer {index}"
        );
        let CursorUpdate::Set(next) = placed.cursor else {
            panic!("placed cursor");
        };
        let &(start, end) = occupied.borrow().last().expect("owned mapping");
        assert_eq!(next, end, "packing follows the OS page, not a hugepage gap");
        assert_eq!(start % page, 0);
        cursor = next;
    }
}

/// Both ENDS have to be in reach, not just the base. A buffer whose start
/// is 1.4GB away and whose end is 2.1GB away is one the emitter can use
/// for its first instruction and not for its last, which is the shape that
/// would make engagement depend on where in the body a guard happens to
/// land.
#[test]
fn a_buffer_is_in_reach_only_when_both_ends_are() {
    let anchor = 0x2000_0000_0000usize;
    assert!(in_reach(anchor + (64 << 20), 4096, anchor), "64MB away");
    assert!(
        !in_reach(anchor + WINDOW - 4096, 1 << 20, anchor),
        "base inside the window and the end outside it must be refused"
    );
    assert!(
        !in_reach(anchor + WINDOW + 1, 4096, anchor),
        "past the window"
    );
}

/// Below the anchor as well as above it. `abs_diff` is what makes that
/// true, and a subtraction written the other way round would silently
/// refuse every placement the kernel puts underneath.
#[test]
fn the_window_is_two_sided() {
    let anchor = 0x2000_0000_0000usize;
    assert!(in_reach(anchor - (64 << 20), 4096, anchor));
    assert!(!in_reach(anchor - WINDOW - (1 << 20), 4096, anchor));
}

/// The window must stay inside the architectural ±2GB `disp32` reaches,
/// with room for the displacement being measured from the END of an
/// instruction somewhere inside the buffer.
#[test]
fn the_window_is_inside_what_disp32_reaches() {
    assert!(
        WINDOW < i32::MAX as usize,
        "the placement window is wider than a RIP displacement can reach, \
         so a buffer this accepts can still emit the long form -- which \
         makes every engagement count a lottery"
    );
}

/// The ladder must probe BELOW the anchor, and early.
///
/// This is the bug the first version shipped with, stated as a test: a
/// walk that only goes up finds nothing on a host whose allocator reserves
/// one 16GB arena with the anchor 511MB inside it, because everything
/// above the anchor within reach is that arena. The free space is
/// underneath. A ladder that reaches down only on its seventh rung would
/// have the same failure with extra steps, so this pins the FIRST rung.
#[test]
fn the_ladder_probes_below_the_anchor_first() {
    let anchor = 0x2001_E810_3F0usize;
    let first = nth_hint(0, 0, 4096, anchor).addr().expect("a first hint");
    assert!(
        first < anchor,
        "the first hint is above the anchor ({first:#x} vs {anchor:#x}), \
         which is the side an arena reserved upward from its base occupies"
    );
    // …and the ladder must get far enough down to clear a half-gigabyte
    // arena prefix within its rungs.
    let deepest = (0..16)
        .filter_map(|i| nth_hint(i, 0, 4096, anchor).addr())
        .filter(|h| *h < anchor)
        .map(|h| anchor - h)
        .max()
        .expect("some rung below");
    assert!(
        deepest >= 1 << 30,
        "the ladder only reaches {deepest} bytes below the anchor; an \
         arena prefix larger than that puts every rung inside it"
    );
}

/// Every rung it offers is one `in_reach` would accept, so a hint is never
/// proposed that the acceptance check would then refuse.
#[test]
fn every_rung_is_inside_the_window() {
    let anchor = 0x2001_E810_3F0usize;
    for i in 0..16 {
        if let Some(h) = nth_hint(i, 0, 1 << 20, anchor).addr() {
            assert!(
                in_reach(h, 1 << 20, anchor),
                "rung {i} proposes {h:#x}, outside the window it is checked against"
            );
        }
    }
}

// -----------------------------------------------------------------------
// The retirement decision.
//
// These five tests exist because of
// `docs/feature-designs/jit-ir-optimizations.md`:
// `nth_hint` returned `Option<usize>`, `place` `break`s on `None`, and the
// only statement after the loop set `RETIRED` — so "this step has no address
// for a buffer this size" and "the ladder is walked out" were the same value
// and had the same consequence, which was switching the strategy off for the
// life of the process.
//
// They drive `walk` and not `place`, and that is the point of `walk`
// existing: `place` reads `enabled()` (a `OnceLock` over an environment
// variable), `RETIRED` (a process-global `AtomicBool` with no reset) and
// `CURSOR`, so from a test it can neither be armed nor observed. `walk`
// returns the decision as a `Walk`, and `map`/`unmap` are already parameters,
// so every assertion below is about pure arithmetic over fabricated
// addresses. Nothing here maps a page or dereferences a pointer.
//
// 64-bit only: the anchors are 46-bit constants, as in the tests above.
// -----------------------------------------------------------------------

/// A ladder rung that has no address for THIS size is not the ladder
/// running out, and neither is a held cursor that cannot fit THIS size.
///
/// The three states are the fix's shape. `Hint::Exhausted` is the only one
/// that means "the ladder is walked out", and before this enum existed
/// `TooBigHere` was spelled the same way and was treated the same way.
#[cfg(target_pointer_width = "64")]
#[test]
fn the_three_reasons_a_step_offers_no_address_are_distinguishable() {
    let anchor = 0x2000_0000_0000usize;

    // `LADDER` has 8 rungs, so with no cursor held step 8 is past its end.
    assert_eq!(
        nth_hint(8, 0, 4096, anchor),
        Hint::Exhausted,
        "step 8 with no cursor is one past the last rung"
    );

    // 3GiB cannot fit at ANY rung: the deepest offset is 1GiB, so the
    // nearest a 3GiB buffer's far end can come to the anchor is 3GiB - 1GiB
    // = 2GiB, and `NEAR_WINDOW` is 1.5GB. Every rung is `TooBigHere` — and
    // `TooBigHere`, not `Exhausted`, because the rung exists and a 4KiB
    // request would be handed its address.
    for i in 0..8 {
        assert_eq!(
            nth_hint(i, 0, 3 << 30, anchor),
            Hint::TooBigHere,
            "rung {i} for a 3GiB request"
        );
    }

    // A cursor whose BASE has left the window is out for every size, so it
    // is permanently absent rather than merely too small for this request.
    assert_eq!(
        nth_hint(0, anchor + WINDOW + (1 << 20), 4096, anchor),
        Hint::NoSuchAddress,
        "a cursor outside the window at its base"
    );
}

/// **The regression test for the filed bug.** A held cursor that cannot fit
/// THIS allocation costs the walk its own step, and the ladder is still
/// walked.
///
/// # Why this fails on the old behaviour
///
/// The cursor below has its base inside the window and its END outside it
/// for a 1MiB request, so the pre-fix `nth_hint(0, cursor, 1MiB, anchor)`
/// returned `None` — `in_reach(cursor, size, anchor).then_some(cursor)`. The
/// pre-fix loop was `let Some(hint) = nth_hint(..) else { break };`, so it
/// left the loop at step 0 having handed nothing to `mmap`, and the only
/// statement after the loop was `RETIRED.store(true, ..)`. On that code this
/// same input produced no placement and retired the strategy, so BOTH
/// assertions below — `placed == Some(rung 0's address)` and
/// `retire == false` — were false, in a way no arrangement of the map
/// closure could change.
///
/// The `map` closure here honours its hint exactly, which is the kernel
/// behaviour the strategy is built for. It is called once, for rung 0, and
/// never for the cursor: `probed == 1` is that claim.
#[cfg(target_pointer_width = "64")]
#[test]
fn a_cursor_too_small_for_this_size_costs_one_step_and_not_the_strategy() {
    let anchor = 0x2000_0000_0000usize;
    let size = 1 << 20;
    // Base 4KiB inside the window; base + 1MiB is 1MiB - 4KiB outside it.
    let cursor = anchor + WINDOW - 4096;
    assert!(
        in_reach(cursor, 0, anchor),
        "the cursor's base must be inside the window, or this test is \
         exercising the permanently-stale case instead"
    );
    assert!(
        !in_reach(cursor, size, anchor),
        "the cursor must NOT fit this size, which is the whole premise"
    );
    assert_eq!(nth_hint(0, cursor, size, anchor), Hint::TooBigHere);

    let maps = std::cell::Cell::new(0usize);
    let unmaps = std::cell::Cell::new(0usize);
    let w = walk(
        size,
        cursor,
        anchor,
        |hint| {
            maps.set(maps.get() + 1);
            Some(hint)
        },
        |_| unmaps.set(unmaps.get() + 1),
    );

    // Rung 0 is `(false, 64 << 20)`: 64MiB BELOW the anchor, and 64MiB is a
    // whole multiple of the 2MiB hint alignment, so the mask does not move
    // it.
    let rung0 = anchor - (64 << 20);
    assert_eq!(
        w.placed,
        Some(rung0 as *mut u8),
        "the ladder must still be walked after the cursor step declines"
    );
    assert!(
        !w.retire,
        "a cursor that cannot fit this size is not evidence that the \
         address space has no room near the anchor"
    );
    assert_eq!(w.attempt, 1, "step 0 was the cursor; rung 0 is step 1");
    assert_eq!(maps.get(), 1, "the declined cursor must cost no syscall");
    assert_eq!(w.probed, 1, "and must not be counted as probed");
    assert_eq!(unmaps.get(), 0, "the placement is kept");
    assert!(!w.map_failed);

    // The cursor follows the page-rounded mapping. A 1MiB buffer is already
    // page-aligned and must not leave another 1MiB gap behind it.
    assert_eq!(
        w.cursor,
        CursorUpdate::Set(anchor - (63 << 20)),
        "a placement seeds the next packing address"
    );
}

/// An allocation too large for any rung must not retire the strategy, and
/// must not cost a single syscall.
///
/// This is the cursor-free half of the same defect: the pre-fix `nth_hint`
/// returned `None` for an out-of-window RUNG too, and the same `break` fell
/// through to the same `RETIRED.store`. So one sufficiently large buffer
/// retired the strategy on the first rung with `cursor == 0`, and every
/// later 64-byte lambda adapter fell back. `retire == false` below is the
/// assertion that flips.
#[cfg(target_pointer_width = "64")]
#[test]
fn a_request_no_rung_can_hold_retires_nothing_and_maps_nothing() {
    let anchor = 0x2000_0000_0000usize;
    let maps = std::cell::Cell::new(0usize);
    let unmaps = std::cell::Cell::new(0usize);
    let w = walk(
        3 << 30,
        0,
        anchor,
        |hint| {
            maps.set(maps.get() + 1);
            Some(hint)
        },
        |_| unmaps.set(unmaps.get() + 1),
    );
    assert_eq!(
        w.placed, None,
        "no rung can hold 3GiB inside a 1.5GB window"
    );
    assert_eq!(
        unmaps.get(),
        0,
        "nothing was mapped, so nothing can be released"
    );
    assert!(
        !w.retire,
        "no rung was probed, so this walk learned nothing about the \
         address space — a smaller buffer must still get a full walk"
    );
    assert_eq!(maps.get(), 0, "an unusable hint costs arithmetic, not mmap");
    assert_eq!(w.probed, 0);
    assert_eq!(
        w.cursor,
        CursorUpdate::Keep,
        "there was no cursor to disturb"
    );
}

/// Retirement still happens, and only for the reason it is written for: the
/// whole ladder was probed and the kernel put every mapping out of reach.
///
/// Without this test the fix above could be "never retire", which would pay
/// eight `mmap`/`munmap` pairs per compile forever on a host with no room
/// near the anchor — the cost retirement exists to avoid.
#[cfg(target_pointer_width = "64")]
#[test]
fn a_ladder_whose_every_rung_is_relocated_out_of_reach_retires() {
    let anchor = 0x2000_0000_0000usize;
    // The measured Linux behaviour this strategy was written against: the
    // kernel ignores the hint and places the mapping in its own region,
    // ~130TB away.
    let elsewhere = 0x7A53_DBCB_8000usize as *mut u8;
    assert!(!in_reach(elsewhere as usize, 4096, anchor));
    let unmaps = std::cell::Cell::new(0usize);
    let w = walk(
        4096,
        0,
        anchor,
        |_| Some(elsewhere),
        |_| unmaps.set(unmaps.get() + 1),
    );
    assert_eq!(w.placed, None);
    assert!(w.retire, "every rung existed, was mapped, and missed");
    assert_eq!(w.probed, 8, "all eight rungs, and the report prints this");
    assert_eq!(unmaps.get(), 8, "every relocated mapping is released");
    assert!(!w.map_failed);
}

/// A cursor whose BASE has left the window is cleared, which is what the
/// comment on the success path has claimed since it was written, and the
/// ladder is still walked in full.
///
/// Before the fix, that cursor produced `None` at step 0 and the walk ended
/// there: nothing was cleared, nothing was probed, and `RETIRED` was set. So
/// `probed == 8` below is a second statement of the same regression, from the
/// permanently-stale side rather than the size-dependent one.
#[cfg(target_pointer_width = "64")]
#[test]
fn a_cursor_outside_the_window_is_cleared_and_does_not_shorten_the_walk() {
    let anchor = 0x2000_0000_0000usize;
    let stale = anchor + WINDOW + (1 << 20);
    let elsewhere = 0x7A53_DBCB_8000usize as *mut u8;
    let w = walk(4096, stale, anchor, |_| Some(elsewhere), |_| {});
    assert_eq!(
        w.cursor,
        CursorUpdate::ClearIfUnchanged,
        "a cursor that can never be in reach again must not be kept"
    );
    assert_eq!(
        w.probed, 8,
        "the unusable cursor cost no syscall and all eight rungs were \
         still walked"
    );
    assert!(w.retire, "and the full walk earns the retirement");
}
