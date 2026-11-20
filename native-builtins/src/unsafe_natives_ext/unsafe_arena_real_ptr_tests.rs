use super::*;

/// **A block resized or freed while native code holds its real pointer is
/// reported.**
///
/// This is the hazard `unsafe_arena_translation_stats` exists for, and it can
/// only be asserted on bookkeeping: the dangling pointer is in a native
/// library's hands, the write happens outside this process's Rust code, and
/// by the time anything notices, the evidence is an unrelated object's
/// corrupted header a collection later. That is the trail
/// `zgc-rewrite-pass-walks-off-a-reference-array-20260815.md` follows
/// backwards.
///
/// # Asserted PER BLOCK, not on the counters
///
/// The counters and the arena store are process-global and cargo runs tests
/// in parallel threads, so a sibling test translating or freeing its own
/// block moves them between any baseline and any assertion. The first
/// version of this asserted exact counter deltas, passed alone, and failed in
/// the full suite -- the ordinary shape of a global-state test, and worth the
/// note. Per-block queries are race-free.
#[test]
fn resizing_or_freeing_a_translated_block_is_reported() {
    let _arena = super::arena_layout_test_lock();
    // A block nobody has translated must NOT be recorded -- a detector that
    // fires on every free names nothing.
    let quiet = unsafe_arena_allocate(64);
    assert!(
        !unsafe_arena_block_is_translated(quiet),
        "an untranslated block must not be recorded"
    );
    let quiet = unsafe_arena_try_reallocate(quiet, 128).expect("resize succeeds");
    assert!(
        !unsafe_arena_block_is_translated(quiet),
        "and resizing it must not make it recorded"
    );
    unsafe_arena_free(quiet);

    // Handing the pointer out records it, and the bound EXISTS here -- both
    // production callers drop it, which is the defect this instruments.
    let held = unsafe_arena_allocate(64);
    let (_p, remaining) = unsafe_arena_real_ptr(held).expect("live handle translates");
    assert_eq!(remaining, 64, "the bound exists at the source");
    assert!(
        unsafe_arena_block_is_translated(held),
        "handing a real pointer to native code must be recorded -- otherwise `reallocate` cannot tell that it is about to move a buffer somebody is holding"
    );

    // Resizing it is the hazard: `Vec::resize` may move the buffer.
    let r_before = unsafe_arena_translation_stats().stale_on_realloc;
    let held = unsafe_arena_try_reallocate(held, 4096).expect("resize succeeds");
    let r_after = unsafe_arena_translation_stats().stale_on_realloc;
    assert!(
        r_after > r_before,
        "resizing a block whose real pointer is outstanding must be reported"
    );

    // Freeing clears the record, so a recycled handle does not inherit it.
    let doomed = unsafe_arena_allocate(64);
    let _ = unsafe_arena_real_ptr(doomed).expect("translates");
    unsafe_arena_free(doomed);
    assert!(
        !unsafe_arena_block_is_translated(doomed),
        "the record must be dropped with the block, or a later handle at the same address inherits a warning that is not about it"
    );

    unsafe_arena_free(held);
}

#[test]
fn a_live_handle_translates_to_a_pointer_that_aliases_the_arena() {
    let _handles = crate::unsafe_natives_ext::handle_mode_store_for_test();
    let _arena = super::arena_layout_test_lock();
    let handle = unsafe_arena_allocate(64);
    assert!(
        unsafe_arena_addr_is_tagged(handle),
        "handle {handle:#x} must carry the arena tag — the whole classification rests on it"
    );

    let (ptr, remaining) = unsafe_arena_real_ptr(handle).expect("live handle translates");
    assert_eq!(remaining, 64);
    assert!(
        !unsafe_arena_addr_is_tagged(ptr as i64),
        "the translated pointer must be a real address, not the handle again"
    );

    // Native-side write, arena-side read: this is the direction that was
    // SIGSEGV-ing, and the aliasing is the point — a copy would pass the
    // write test and still leave the buffer's contents wrong.
    // SAFETY: `remaining` says 64 bytes are owned by this block.
    unsafe {
        std::ptr::write(ptr.add(3), 0xAB);
    }
    assert_eq!(unsafe_arena_get_byte(handle + 3), 0xAB);

    // Arena-side write, native-side read: the other direction.
    assert!(unsafe_arena_put_byte(handle + 5, 0xCD));
    // SAFETY: same block, offset 5 < 64.
    assert_eq!(unsafe { std::ptr::read(ptr.add(5)) }, 0xCD);

    unsafe_arena_free(handle);
}

#[test]
fn a_mid_block_handle_translates_to_the_matching_interior_pointer() {
    let _arena = super::arena_layout_test_lock();
    let handle = unsafe_arena_allocate(32);
    let (base, base_len) = unsafe_arena_real_ptr(handle).expect("base translates");
    let (mid, mid_len) = unsafe_arena_real_ptr(handle + 8).expect("interior translates");
    assert_eq!(base_len, 32);
    assert_eq!(mid_len, 24, "remaining must count from the interior offset");
    assert_eq!(mid as usize - base as usize, 8);
    unsafe_arena_free(handle);
}

#[test]
fn a_real_pointer_and_a_dead_handle_both_refuse() {
    let _arena = super::arena_layout_test_lock();
    // Untagged: already a real pointer (a native's own NewDirectByteBuffer
    // allocation). Nothing to translate, and translating would be wrong.
    let mut real = [0u8; 8];
    assert!(unsafe_arena_real_ptr(real.as_mut_ptr() as i64).is_none());

    // Freed: a use-after-free must not be handed a pointer into whatever
    // the allocator reused, so it resolves to None and JNI answers NULL.
    let handle = unsafe_arena_allocate(16);
    assert!(unsafe_arena_real_ptr(handle).is_some());
    unsafe_arena_free(handle);
    assert!(unsafe_arena_real_ptr(handle).is_none());

    // Past the end of a live block: same refusal, so a native cannot walk
    // off one buffer into the next. Size 8 on purpose — `allocate` rounds
    // the address bump to 16, so `live + 8 .. live + 15` is guaranteed to
    // belong to no block at all, even though this arena is process-global
    // and other tests allocate into it concurrently.
    let live = unsafe_arena_allocate(8);
    assert!(unsafe_arena_real_ptr(live + 8).is_none());
    unsafe_arena_free(live);
}

/// **Growing a block past its reserved handle space must not swallow the
/// handles issued after it.**
///
/// `try_allocate` bumps `next_addr` by the rounded request, so a block owns
/// exactly that much handle space, and `locate` resolves an address to the
/// greatest base at or below it. Growing `bytes` past that made the block
/// answer for its successors' handles: a 64-byte block grown to 4096
/// swallowed the next block whole, and every read and write through the
/// later handle landed silently in the earlier block. Two live
/// `Unsafe.allocateMemory` allocations, one address range.
///
/// The write-through-both-and-read-back-both shape is deliberate: a test
/// that only checks the later block reads its own value passes while the
/// earlier block is being corrupted underneath it.
#[test]
fn growing_a_block_must_not_swallow_the_handles_issued_after_it() {
    let _handles = crate::unsafe_natives_ext::handle_mode_store_for_test();
    let _arena = super::arena_layout_test_lock();
    let a = unsafe_arena_allocate(64);
    let b = unsafe_arena_allocate(64);
    assert!(b > a, "handles bump upward");
    // Sized from the ACTUAL gap rather than a constant: this arena is
    // process-global and cargo runs tests in parallel, so a sibling's
    // allocation lands between these two and a fixed 4096 stops spanning
    // `b`. A precondition that a race can make vacuous is worse than none.
    //
    // nio21 wave 1 L3: two SLOTTED handles are a whole 2^30-byte window
    // apart, so the gap no longer measures a grow that reaches `b` (and
    // would allocate a gigabyte); any grow past `a`'s 64 reserved bytes is
    // the case then, and 4096 is one.
    let gap = (b - a) as usize;
    let span = if gap < (1 << 20) { gap + 128 } else { 4096 };

    let grown = unsafe_arena_try_reallocate(a, span).expect("resize succeeds");
    assert_ne!(
        grown, a,
        "a grow that its reserved handle space cannot hold must relocate"
    );

    assert!(unsafe_arena_put_byte(b + 1, 0x77));
    assert!(unsafe_arena_put_byte(grown + 1, 0x11));
    assert_eq!(
        unsafe_arena_get_byte(b + 1),
        0x77,
        "b must keep its own bytes"
    );
    assert_eq!(
        unsafe_arena_get_byte(grown + 1),
        0x11,
        "and the grown block must keep its own"
    );

    // The old handle is dead, which is `realloc(3)`'s contract and what
    // keeps a use-after-realloc resolving to nothing rather than to
    // whichever block now covers that address.
    assert!(unsafe_arena_real_ptr(a).is_none());
    assert!(!unsafe_arena_put_byte(a, 0xFF));

    unsafe_arena_free(grown);
    unsafe_arena_free(b);
}

/// **A grow that DOES fit the reserved handle space keeps its handle.** The
/// negative half: relocating every grow would be correct and would also
/// make the test above pass for the wrong reason, while needlessly
/// invalidating handles Java is entitled to keep using.
#[test]
fn a_grow_inside_the_reserved_handle_space_keeps_its_handle() {
    let _handles = crate::unsafe_natives_ext::handle_mode_store_for_test();
    let _arena = super::arena_layout_test_lock();
    // 60 bytes reserves 64 -- `next_addr` bumps by the 16-rounded request.
    let handle = unsafe_arena_allocate(60);
    assert!(unsafe_arena_put_byte(handle + 3, 0x2B));
    let same = unsafe_arena_try_reallocate(handle, 64).expect("resize succeeds");
    assert_eq!(same, handle, "a grow that fits must not move the handle");
    assert_eq!(unsafe_arena_get_byte(handle + 3), 0x2B);
    unsafe_arena_free(handle);
}

/// **A move under an outstanding real pointer keeps the displaced buffer
/// alive, and the outstanding pointer still reads it.**
///
/// The assertion that matters is the read through the stale pointer AFTER
/// the move. Before the retention it named memory the process allocator had
/// taken back, so the read was undefined and a write through it was an
/// unbounded corruption of whatever landed there next; now it names a
/// buffer this process still owns, still holding the pre-move bytes.
///
/// Run this one under Miri or ASan and the difference is the whole point:
/// the old code is a use-after-free here, this one is not.
///
/// # Asserted PER BLOCK wherever possible
///
/// The counters are process-global and cargo runs tests in parallel, so a
/// sibling moving its own block lands between any baseline and any
/// assertion. Counter checks are therefore `>=`, and the load-bearing
/// assertions are per-block queries, which are race-free. The module's
/// first attempt at exact deltas passed alone and failed in the suite.
#[test]
fn a_move_under_an_outstanding_pointer_retains_the_displaced_buffer() {
    let _arena = super::arena_layout_test_lock();
    let handle = unsafe_arena_allocate(64);
    assert!(unsafe_arena_put_byte(handle + 7, 0x5A));
    let (ptr, remaining) = unsafe_arena_real_ptr(handle).expect("live handle translates");
    assert_eq!(remaining, 64);

    let retained_before = unsafe_arena_translation_stats().retained_on_realloc;
    let moved = unsafe_arena_try_reallocate(handle, 1 << 20).expect("resize succeeds");
    assert_ne!(
        moved, handle,
        "1 MiB cannot fit the 64 bytes of handle space"
    );
    assert!(
        unsafe_arena_translation_stats().retained_on_realloc > retained_before,
        "a move under an outstanding pointer must retain the displaced buffer"
    );
    assert!(
        unsafe_arena_block_is_retired(moved),
        "the retained buffer must be keyed by the handle the block ended up at, \
         or `free` cannot drop it"
    );
    assert!(
        !unsafe_arena_block_is_retired(handle),
        "and not by the dead one, which nothing will ever free"
    );

    // The stale pointer still names owned memory holding the old bytes.
    // SAFETY: this is exactly the dereference the retention makes sound --
    // the displaced buffer is alive and at least 64 bytes long.
    assert_eq!(unsafe { std::ptr::read(ptr.add(7)) }, 0x5A);

    // The block itself moved on, and carried its contents.
    assert_eq!(unsafe_arena_get_byte(moved + 7), 0x5A);

    unsafe_arena_free(moved);
    assert!(
        !unsafe_arena_block_is_retired(moved),
        "freeing the handle must drop the retained buffer -- otherwise an \
         allocate/translate/resize/free loop leaks one buffer per iteration"
    );
}

/// **A resize that cannot move the buffer retains nothing.** The negative
/// half: without it the retention fires on every resize of every translated
/// block, and the count says nothing about the hazard.
#[test]
fn a_resize_that_cannot_move_the_buffer_retains_nothing() {
    let _arena = super::arena_layout_test_lock();
    let handle = unsafe_arena_allocate(4096);
    let _ = unsafe_arena_real_ptr(handle).expect("translates");
    // Shrinking never reallocates and fits the reserved space, so the block
    // neither moves in memory nor changes handle.
    let same = unsafe_arena_try_reallocate(handle, 16).expect("resize succeeds");
    assert_eq!(same, handle);
    assert!(
        !unsafe_arena_block_is_retired(handle),
        "a shrink displaces nothing, so there is nothing to retain"
    );
    unsafe_arena_free(handle);
}

/// **The resize path must not move a translated block before it can notice.**
///
/// `try_reallocate`'s capacity probe used to run `try_reserve_exact` on the
/// LIVE `Vec`, which reallocates -- so the buffer moved during the probe,
/// and the detection downstream then read a capacity that had just been
/// satisfied and concluded nothing had moved. An instrument armed where it
/// could not fire. The probe is gone; this is what proves it stays gone.
#[test]
fn the_resize_path_does_not_move_a_translated_block_before_noticing() {
    let _arena = super::arena_layout_test_lock();
    let handle = unsafe_arena_allocate(64);
    let (ptr, _) = unsafe_arena_real_ptr(handle).expect("translates");
    // SAFETY: 64 bytes are owned by this block.
    unsafe { std::ptr::write(ptr.add(1), 0xC3) };

    let before = unsafe_arena_translation_stats().retained_on_realloc;
    let moved = unsafe_arena_try_reallocate(handle, 1 << 20).expect("resize succeeds");
    assert!(
        unsafe_arena_translation_stats().retained_on_realloc > before,
        "the growth must be detected as a move, not hidden by a probe"
    );
    assert!(unsafe_arena_block_is_retired(moved));
    // SAFETY: sound because the displaced buffer was retained.
    assert_eq!(unsafe { std::ptr::read(ptr.add(1)) }, 0xC3);
    unsafe_arena_free(moved);
}

/// **A bounded translation refuses a block that cannot cover the advertised
/// length, and accepts one that can.**
///
/// Both halves are asserted: a guard that refuses everything is as useless
/// as one that refuses nothing, and `remaining == want` is the boundary the
/// JNI contract actually names -- a native may touch `capacity` bytes, not
/// `capacity + 1`.
#[test]
fn a_bounded_translation_refuses_only_a_block_that_is_too_short() {
    let _arena = super::arena_layout_test_lock();
    let handle = unsafe_arena_allocate(64);
    assert!(
        unsafe_arena_real_ptr_bounded(handle, 64).is_some(),
        "a block that exactly covers the advertised length must be published"
    );
    assert!(
        unsafe_arena_real_ptr_bounded(handle, 0).is_some(),
        "a zero-length buffer is legal and must not be refused"
    );

    let short_before = unsafe_arena_translation_stats().short_translations;
    assert!(
        unsafe_arena_real_ptr_bounded(handle, 65).is_none(),
        "one byte past the block must be refused -- that byte is the bug"
    );
    assert!(
        unsafe_arena_translation_stats().short_translations > short_before,
        "and the refusal must be counted, or the guard is invisible"
    );

    // From an interior handle the bound counts from the offset, so the same
    // block refuses a length it accepted from its base.
    assert!(unsafe_arena_real_ptr_bounded(handle + 32, 32).is_some());
    assert!(unsafe_arena_real_ptr_bounded(handle + 32, 33).is_none());

    unsafe_arena_free(handle);
    assert!(
        unsafe_arena_real_ptr_bounded(handle, 1).is_none(),
        "a dead handle refuses regardless of length"
    );
}
