use super::*;

// -- protect-failure classification (r9 wave 4, lib4) --------------------

/// Only a policy denial may latch the process-wide "JIT unavailable"
/// switch; `ENOMEM`/`EINVAL` (and the uninformative `-1`) decline one
/// compile.
///
/// The `JitError::AllocationFailed` case this used to assert is gone with
/// the variant (round 10 wave 8): the assertion was
/// `assert!(!AllocationFailed.is_policy_denial())` on a value nothing in the
/// workspace could construct, so it tested a case that could not occur while
/// reading like coverage of one that could.
#[test]
fn only_policy_refusals_classify_as_policy_denials() {
    assert!(!JitError::ProtectFailed(-1).is_policy_denial());
    if cfg!(target_os = "windows") {
        assert!(JitError::ProtectFailed(1655).is_policy_denial());
        assert!(!JitError::ProtectFailed(5).is_policy_denial());
        assert!(!JitError::ProtectFailed(87).is_policy_denial()); // INVALID_PARAMETER
        assert!(!JitError::ProtectFailed(8).is_policy_denial()); // NOT_ENOUGH_MEMORY
    } else {
        assert!(JitError::ProtectFailed(13).is_policy_denial()); // EACCES
        assert!(JitError::ProtectFailed(1).is_policy_denial()); // EPERM
        assert!(!JitError::ProtectFailed(12).is_policy_denial()); // ENOMEM
        assert!(!JitError::ProtectFailed(22).is_policy_denial()); // EINVAL
    }
}

// -- the code arena ----------------------------------------------------

/// An arena with eight-page regions.
///
/// Small on purpose: the tests want "larger than a region" to be a cheap
/// request and want to see a second region get mapped without allocating
/// 32 MiB to do it. A test arena that is dropped without emptying leaks the
/// regions it still holds — there is no `Drop`, for the reason
/// [`JitCodeArena`] gives — and eight pages apiece is why that is a
/// rounding error rather than a leak worth caring about.
///
/// These arenas are NOT the process-wide one. Nothing here calls
/// [`shared_jit_code_arena`], and nothing here publishes spans (that
/// happens in `ExecutableBuffer::new_in`), so a test arena's regions are
/// invisible to `free_if_arena` and two tests running concurrently cannot
/// see each other's blocks.
fn test_arena() -> JitCodeArena {
    JitCodeArena::with_region_bytes(code_page_size() * 8)
}

/// Fill a region's eight pages with eight one-page blocks.
///
/// Used by the reclamation tests, which need "a region that is exactly
/// full" to be a precise thing rather than an approximation: a region with
/// bump room left is reclaimable on a different argument (it never handed
/// those bytes out) and would not exercise the `free_bytes ==
/// handed_out()` half of [`Region::is_reclaimable`].
fn fill_one_region(arena: &mut JitCodeArena) -> Vec<ArenaBlock> {
    let page = code_page_size();
    let before = arena.census().regions;
    let blocks: Vec<ArenaBlock> = (0..8).map(|_| arena.alloc(page).expect("fill")).collect();
    assert_eq!(
        arena.census().regions,
        before + 1,
        "eight one-page blocks should fill exactly one new eight-page region"
    );
    blocks
}

/// The carve: several blocks out of ONE region, pairwise disjoint.
///
/// Disjointness is checked pairwise and not merely "the addresses differ",
/// because the failure the arena must not have is two blocks OVERLAPPING —
/// distinct bases with intersecting extents, which a bump cursor that
/// advanced by the request rather than the rounded size would produce.
#[test]
fn an_arena_serves_several_blocks_from_one_region() {
    if JIT_CODE_ARENA_IS_INERT {
        // Inert by design here: every allocation is its own mapping and
        // there is no region to carve. See `JIT_CODE_ARENA_IS_INERT`.
        return;
    }
    let mut arena = test_arena();
    let page = code_page_size();
    let blocks: Vec<ArenaBlock> = (0..4)
        .map(|_| arena.alloc(page / 2).expect("arena alloc"))
        .collect();

    let census = arena.census();
    assert_eq!(
        census.regions, 1,
        "four half-page requests round to four pages and must fit one \
         eight-page region; {} regions were mapped",
        census.regions
    );
    assert_eq!(census.served_from_bump, 4);
    assert_eq!(census.served_standalone, 0);
    for b in &blocks {
        assert_eq!(
            b.origin(),
            BlockOrigin::Region(0),
            "block {:#x} did not come from the one region",
            b.as_ptr() as usize
        );
    }
    for (i, a) in blocks.iter().enumerate() {
        for b in blocks.iter().skip(i + 1) {
            let (a_lo, a_hi) = (a.as_ptr() as usize, a.as_ptr() as usize + a.size());
            let (b_lo, b_hi) = (b.as_ptr() as usize, b.as_ptr() as usize + b.size());
            assert!(
                a_hi <= b_lo || b_hi <= a_lo,
                "blocks [{a_lo:#x},{a_hi:#x}) and [{b_lo:#x},{b_hi:#x}) overlap"
            );
        }
    }
    for b in blocks {
        arena.free(b);
    }
}

/// Recycling happens, and it happens only within a size class.
///
/// The second half is the one with teeth: a free list that answered a
/// larger request from a smaller bucket would hand a two-page method a
/// one-page block, and the overrun would land in whatever the next block
/// is — silently, because `emit` bounds-checks against `capacity` and
/// `capacity` would be the lie.
///
/// The direction matters and only one of the two is allowed. A SMALLER
/// request may now be served from a LARGER free block, by splitting it —
/// that is `a_split_block_serves_a_smaller_request_and_its_remainder_is_
/// reusable`. A larger request from a smaller block is the bug, and this
/// test is what stands between the two.
#[test]
fn a_freed_block_is_reused_and_a_larger_request_is_not_served_from_it() {
    if JIT_CODE_ARENA_IS_INERT {
        return;
    }
    let mut arena = test_arena();
    let page = code_page_size();

    let one = arena.alloc(page).expect("one-page alloc");
    let recycled_addr = one.as_ptr() as usize;
    arena.free(one);
    assert_eq!(arena.census().bytes_free_listed, page);

    let two = arena.alloc(page + 1).expect("two-page alloc");
    assert_eq!(two.size(), page * 2);
    assert_ne!(
        two.as_ptr() as usize,
        recycled_addr,
        "a two-page request was served from the one-page free list"
    );
    assert_eq!(
        arena.census().served_from_free_list,
        0,
        "nothing should have come off a free list yet"
    );
    assert_eq!(
        arena.census().bytes_free_listed,
        page,
        "the one-page block must still be on its own list"
    );

    let again = arena.alloc(page).expect("one-page alloc again");
    assert_eq!(
        again.as_ptr() as usize,
        recycled_addr,
        "the freed one-page block was not reused"
    );
    assert_eq!(arena.census().served_from_free_list, 1);
    assert_eq!(arena.census().bytes_free_listed, 0);

    arena.free(two);
    arena.free(again);
}

/// Page alignment and page sizing — asserted, because the whole W^X
/// argument in this module's arena section rests on them.
///
/// If a block were merely page-ALIGNED and not page-SIZED, the next block
/// would start mid-page and `make_executable` on the first would flip the
/// second's first page too.
#[test]
fn every_arena_block_is_page_sized_and_page_aligned() {
    if JIT_CODE_ARENA_IS_INERT {
        return;
    }
    let mut arena = test_arena();
    let page = code_page_size();
    for request in [1usize, 17, page - 1, page, page + 1, page * 3 + 7] {
        let block = arena.alloc(request).expect("arena alloc");
        let addr = block.as_ptr() as usize;
        assert_eq!(
            addr % page,
            0,
            "block {addr:#x} for a {request}-byte request is not page-aligned, \
             so it can share a page with its neighbour"
        );
        assert_eq!(
            block.size() % page,
            0,
            "block {addr:#x} is {} bytes, not a whole number of {page}-byte pages",
            block.size()
        );
        assert!(block.size() >= request, "a block must cover its request");
        assert!(
            block.size() < request + page,
            "a {request}-byte request took {} bytes; rounding must cost less \
             than one page",
            block.size()
        );
        assert_eq!(block.requested(), request);
        arena.free(block);
    }
}

/// A request no region can hold gets its own mapping instead of `None`.
///
/// Answering `None` would be a compile refused for a reason the caller
/// cannot act on; the pre-arena allocator would have served it, so the
/// arena must too. Runs on every platform, including the inert one, where
/// this is the ONLY path.
#[test]
fn an_allocation_larger_than_a_region_falls_back_rather_than_failing() {
    let mut arena = test_arena();
    let oversize = arena.region_bytes() + 1;
    let block = arena
        .alloc(oversize)
        .expect("an over-region request must fall back to a standalone mapping");
    assert_eq!(block.origin(), BlockOrigin::Standalone);
    assert!(block.size() >= oversize);
    let census = arena.census();
    assert_eq!(census.served_standalone, 1);
    assert_eq!(
        census.regions, 0,
        "a request that cannot fit a region must not have mapped one"
    );
    arena.free(block);
}

/// Every reserved byte is accounted for, in exactly one place.
///
/// A census whose parts do not sum is worse than no census: it will be used
/// to argue that the arena does or does not pay for itself, and it will be
/// wrong by whatever it silently drops.
#[test]
fn the_census_partitions_every_reserved_byte() {
    if JIT_CODE_ARENA_IS_INERT {
        return;
    }
    let mut arena = test_arena();
    let page = code_page_size();
    let a = arena.alloc(page / 2 + 1).expect("alloc a");
    let b = arena.alloc(page * 2 - 3).expect("alloc b");
    let c = arena.alloc(7).expect("alloc c");
    arena.free(b);

    let census = arena.census();
    assert_eq!(
        census.bytes_requested_live
            + census.bytes_padding_live
            + census.bytes_free_listed
            + census.bytes_never_bumped
            + census.bytes_stranded,
        census.bytes_usable,
        "the census does not partition the usable bytes: {census:?}"
    );
    assert_eq!(
        census.bytes_stranded, 0,
        "nothing here should have failed to go back to writable: {census:?}"
    );
    assert_eq!(
        census.bytes_usable + census.bytes_alignment_waste,
        census.bytes_mapped,
        "mapped bytes are not usable bytes plus alignment waste: {census:?}"
    );
    assert_eq!(
        census.bytes_live,
        census.bytes_requested_live + census.bytes_padding_live
    );
    assert_eq!(census.live_blocks, 2);
    assert_eq!(census.bytes_live, page * 2, "a and c are one page each");
    assert_eq!(census.bytes_free_listed, page * 2, "b was two pages");
    assert_eq!(census.bytes_never_bumped, page * 4, "eight pages less four");
    assert_eq!(
        census.bytes_requested_live,
        (page / 2 + 1) + 7,
        "the requested total must be what the callers asked for, not what \
         they were given"
    );

    arena.free(a);
    arena.free(c);
    let census = arena.census();
    assert_eq!(census.bytes_live, 0);
    assert_eq!(
        census.bytes_requested_live
            + census.bytes_padding_live
            + census.bytes_free_listed
            + census.bytes_never_bumped
            + census.bytes_stranded,
        census.bytes_usable,
        "the identity must survive every block being returned: {census:?}"
    );
    // The region is now empty, and the hysteresis rule kept it: one empty
    // region is the spare. So `bytes_usable` is still a region's worth and
    // the identity above is over something rather than over nothing, which
    // is the only reason this assertion is here.
    assert_eq!(census.regions, 1, "the spare must survive: {census:?}");
    assert_eq!(census.empty_regions, 1);
    assert_eq!(census.regions_reclaimed, 0);
    assert_eq!(census.bytes_usable, page * 8);
}

/// A recycled block is WRITABLE, even though the method that had it before
/// left it executable.
///
/// This is the one arena test that would have failed with an access
/// violation rather than an assertion before `release` learned to restore
/// the protection. `ExecutableBuffer::drop` never calls `make_writable` —
/// it had no reason to, because before the arena a retired mapping went
/// back to the OS and the next buffer got fresh RW pages — so a block that
/// had been through `finalize` came back to the free list still RX, and the
/// next compile's first `emit` would have faulted. The `finalize` here is
/// what makes the test mean anything: without it the block would be RW by
/// accident and the write would prove nothing.
#[test]
fn a_recycled_block_is_writable_again_after_its_predecessor_was_executable() {
    if JIT_CODE_ARENA_IS_INERT {
        // No per-page protection to restore: W^X there is the thread-wide
        // `pthread_jit_write_protect_np` toggle, and the arena is inert
        // anyway. See `JIT_CODE_ARENA_IS_INERT`.
        return;
    }
    let mut arena = test_arena();
    let page = code_page_size();

    let first = arena.alloc(page).expect("first block");
    let addr = first.as_ptr() as usize;
    {
        let _write = JitWriteScope::enter();
        // SAFETY: a live block this arena handed out, still RW from its
        // region's initial mapping, and one byte is inside it.
        unsafe { *first.as_ptr() = 0x90 };
    }
    // Exactly what a real compile does at the end of codegen.
    make_executable(first.as_ptr(), first.size()).expect("make_executable");
    arena.free(first);

    let second = arena.alloc(page).expect("second block");
    assert_eq!(
        second.as_ptr() as usize,
        addr,
        "the one-page block must have been recycled, or this test is \
         writing into fresh memory and proving nothing"
    );
    {
        let _write = JitWriteScope::enter();
        // SAFETY: a live block this arena handed out. It is writable
        // because `release` restored the protection the `make_executable`
        // above took away — which is the whole assertion. If that ever
        // regresses this line faults instead of failing, which is why the
        // address check above comes first: it proves the block really is
        // the recycled one.
        unsafe { *second.as_ptr() = 0xCC };
    }
    assert_eq!(arena.census().served_from_free_list, 1);
    assert_eq!(
        arena.census().stranded_blocks,
        0,
        "the protection flip must have succeeded, not stranded the block"
    );
    arena.free(second);
}

// -- region reclamation ------------------------------------------------

/// A region whose every block has been freed is unmapped — but only once a
/// SECOND region has gone empty, because one empty region is kept as a
/// spare.
///
/// Both halves matter and they pull in opposite directions, which is why
/// they are asserted in one test rather than two. Without reclamation a run
/// that compiles a burst and quiesces holds every region forever; with
/// EAGER reclamation an arena that oscillates around one live method
/// unmaps and re-maps a region per method, which is worse than either. The
/// rule is [`JIT_CODE_ARENA_EMPTY_REGION_SPARES`] and this test pins both
/// its trigger (more than one empty) and its floor (reclaim down to one).
#[test]
fn an_empty_region_is_reclaimed_but_one_spare_is_kept() {
    if JIT_CODE_ARENA_IS_INERT {
        return;
    }
    let mut arena = test_arena();
    let page = code_page_size();

    let first = fill_one_region(&mut arena);
    let second = fill_one_region(&mut arena);
    assert_eq!(arena.census().regions, 2);
    assert_eq!(arena.census().live_blocks, 16);
    assert_eq!(
        arena.census().empty_regions,
        0,
        "both regions are entirely live"
    );

    // Empty the FIRST region completely. One empty region is the spare, so
    // nothing may be unmapped yet — this is the half that a naive "free the
    // region as soon as it empties" implementation gets wrong.
    for b in first {
        arena.free(b);
    }
    let census = arena.census();
    assert_eq!(
        census.regions, 2,
        "the first region to empty is the spare and must NOT be unmapped: \
         {census:?}"
    );
    assert_eq!(census.empty_regions, 1);
    assert_eq!(census.regions_reclaimed, 0);
    assert_eq!(census.bytes_free_listed, page * 8);

    // Empty the second. Now two are empty, which is one more than the rule
    // allows, so exactly one is reclaimed and exactly one is left.
    for b in second {
        arena.free(b);
    }
    let census = arena.census();
    assert_eq!(
        census.regions, 1,
        "with two empty regions one must be reclaimed: {census:?}"
    );
    assert_eq!(census.regions_reclaimed, 1);
    assert_eq!(
        census.bytes_reclaimed as usize,
        page * 8,
        "the reclaimed region's whole mapping is accounted for: {census:?}"
    );
    assert_eq!(census.empty_regions, 1, "the spare stays empty and mapped");
    assert_eq!(census.live_blocks, 0);
    assert_eq!(
        census.bytes_free_listed,
        page * 8,
        "only the surviving region's blocks may still be on a free list: \
         {census:?}"
    );
    assert_eq!(
        census.bytes_requested_live
            + census.bytes_padding_live
            + census.bytes_free_listed
            + census.bytes_never_bumped
            + census.bytes_stranded,
        census.bytes_usable,
        "the partition must hold across a reclamation: {census:?}"
    );
}

/// No free-list entry may survive its region.
///
/// This is the one that would be a wild write rather than a failed
/// assertion if it regressed: an entry left behind by
/// [`JitCodeArena::reclaim_region`] names an address inside memory that
/// `platform_free` has just returned to the OS, and the next allocation of
/// that size class would pop it and hand it to a compiler as a place to
/// write instructions. So the check is made against the reclaimed region's
/// actual address range and not merely against a count.
#[test]
fn a_reclaimed_regions_blocks_are_gone_from_the_free_list() {
    if JIT_CODE_ARENA_IS_INERT {
        return;
    }
    let mut arena = test_arena();
    let page = code_page_size();

    let first = fill_one_region(&mut arena);
    let second = fill_one_region(&mut arena);
    let second_lo = second
        .iter()
        .map(|b| b.as_ptr() as usize)
        .min()
        .expect("eight blocks");
    let second_hi = second
        .iter()
        .map(|b| b.as_ptr() as usize + b.size())
        .max()
        .expect("eight blocks");
    let first_lo = first
        .iter()
        .map(|b| b.as_ptr() as usize)
        .min()
        .expect("eight blocks");

    for b in first {
        arena.free(b);
    }
    for b in second {
        arena.free(b);
    }
    assert_eq!(arena.census().regions_reclaimed, 1);

    // The surviving region is the lower-indexed one — both have zero bump
    // room, so the spare rule's tie-break picks the lowest slot — so the
    // reclaimed range is the second region's.
    let surviving: Vec<(usize, usize)> = arena
        .free_by_pages
        .iter()
        .flat_map(|list| list.iter().copied())
        .collect();
    assert_eq!(
        surviving.len(),
        8,
        "only the spare's eight one-page blocks may remain: {surviving:?}"
    );
    for &(addr, region) in &surviving {
        assert!(
            addr < second_lo || addr >= second_hi,
            "free entry {addr:#x} lies inside the reclaimed region \
             [{second_lo:#x},{second_hi:#x}) — it points at unmapped memory"
        );
        assert!(
            addr >= first_lo && addr < first_lo + page * 8,
            "free entry {addr:#x} is not inside the surviving region either"
        );
        assert!(
            arena.regions[region].is_some(),
            "free entry {addr:#x} names reclaimed region slot {region}"
        );
    }

    // And the slot the reclaimed region vacated is REUSED rather than
    // appended past: the next region that has to be mapped takes it, so the
    // slot vector does not grow without bound over a long run. The request
    // is a whole region's worth on purpose — anything smaller would be
    // served from the spare's free list and map nothing.
    let slots_before = arena.regions.len();
    let whole = arena.alloc(page * 8).expect("a whole region's worth");
    assert_eq!(
        arena.census().regions,
        2,
        "an eight-page request fits no free block and no bump cursor, so a \
         region had to be mapped"
    );
    assert_eq!(
        arena.regions.len(),
        slots_before,
        "a vacated region slot must be reused, not appended past"
    );
    arena.free(whole);
}

/// A region that is still handing blocks out is never reclaimed, even when
/// other regions are empty.
///
/// The rule the arena must keep is not "reclaim empty regions" but "never
/// unmap a region anything is still using". A single live block anywhere in
/// a region pins the whole 16 MiB (8 pages here), and that is correct: the
/// unit of unmapping is the mapping.
#[test]
fn a_region_with_one_live_block_is_never_reclaimed() {
    if JIT_CODE_ARENA_IS_INERT {
        return;
    }
    let mut arena = test_arena();
    let page = code_page_size();

    let first = fill_one_region(&mut arena);
    let mut second = fill_one_region(&mut arena);
    let third = fill_one_region(&mut arena);
    assert_eq!(arena.census().regions, 3);

    // Keep exactly one block of the middle region alive.
    let pinned = second.pop().expect("eight blocks");

    for b in first {
        arena.free(b);
    }
    for b in second {
        arena.free(b);
    }
    for b in third {
        arena.free(b);
    }

    let census = arena.census();
    assert_eq!(census.live_blocks, 1, "only the pinned block is live");
    assert_eq!(
        census.regions, 2,
        "two of three regions were empty, so exactly one is reclaimed and \
         one is kept as the spare; the pinned region is neither: {census:?}"
    );
    assert_eq!(census.regions_reclaimed, 1);
    assert_eq!(
        census.empty_regions, 1,
        "the pinned region does not count as empty: {census:?}"
    );
    // The pinned block is still usable: its region is still mapped, so a
    // write into it is defined. This is the assertion that would fault
    // rather than fail if a region with a live block were ever unmapped.
    {
        let _write = JitWriteScope::enter();
        // SAFETY: `pinned` is a live block this arena handed out and never
        // took back; one byte is inside it, and it has not been made
        // executable, so it is still writable.
        unsafe { *pinned.as_ptr() = 0x90 };
    }
    assert_eq!(
        census.bytes_free_listed,
        page * 8 + page * 7,
        "the spare's eight pages plus the pinned region's seven freed ones: \
         {census:?}"
    );
    arena.free(pinned);
}

// -- splitting ---------------------------------------------------------

/// A larger free block serves a smaller request, and its remainder is
/// reusable.
///
/// This is what exact-fit-only could not do: before splitting, a 4-page
/// block on the free list was dead weight to every request that was not for
/// exactly 4 pages, so a workload whose method sizes drifted stranded a
/// class at a time. Both pieces are checked for page alignment and page
/// sizing, because the W^X argument at the top of this file is an induction
/// and a split is the step that could break it.
#[test]
fn a_split_block_serves_a_smaller_request_and_its_remainder_is_reusable() {
    if JIT_CODE_ARENA_IS_INERT {
        return;
    }
    let mut arena = test_arena();
    let page = code_page_size();

    let big = arena.alloc(page * 4).expect("four-page alloc");
    let big_addr = big.as_ptr() as usize;
    assert_eq!(big.size(), page * 4);
    arena.free(big);
    assert_eq!(arena.census().bytes_free_listed, page * 4);
    assert_eq!(arena.census().free_blocks, 1);

    // One page out of the four-page block: the front piece, at the same
    // address, with three pages pushed back as a new class-3 entry.
    let small = arena.alloc(page).expect("one-page alloc");
    assert_eq!(
        small.as_ptr() as usize,
        big_addr,
        "a one-page request must be served from the head of the four-page \
         free block, not from the bump cursor"
    );
    assert_eq!(small.size(), page);
    let census = arena.census();
    assert_eq!(census.blocks_split, 1);
    assert_eq!(census.served_from_free_list, 1);
    assert_eq!(
        census.served_from_bump, 1,
        "only the original four-page block came off the cursor: {census:?}"
    );
    assert_eq!(
        census.bytes_free_listed,
        page * 3,
        "the remainder must be back on a free list: {census:?}"
    );
    assert_eq!(
        census.free_blocks, 1,
        "one remainder, not three: {census:?}"
    );

    // The remainder is reusable, at exactly the address the split left it.
    let rest = arena.alloc(page * 3).expect("three-page alloc");
    assert_eq!(
        rest.as_ptr() as usize,
        big_addr + page,
        "the remainder must be handed out at the split point"
    );
    assert_eq!(rest.size(), page * 3);
    assert_eq!(arena.census().served_from_free_list, 2);
    assert_eq!(arena.census().blocks_split, 1, "the second fit exactly");
    assert_eq!(arena.census().bytes_free_listed, 0);

    // Both pieces: page-aligned, page-sized, disjoint, and together exactly
    // the block they were cut from.
    for piece in [&small, &rest] {
        let addr = piece.as_ptr() as usize;
        assert_eq!(addr % page, 0, "split piece {addr:#x} is not page-aligned");
        assert_eq!(
            piece.size() % page,
            0,
            "split piece {addr:#x} is {} bytes, not whole pages",
            piece.size()
        );
    }
    let small_hi = small.as_ptr() as usize + small.size();
    assert_eq!(small_hi, rest.as_ptr() as usize, "the pieces must abut");
    assert_eq!(
        rest.as_ptr() as usize + rest.size(),
        big_addr + page * 4,
        "the pieces must together cover exactly the original block"
    );

    arena.free(small);
    arena.free(rest);
}

/// Splitting narrows the free lists and nothing widens them again.
///
/// The honest half of the splitting change: there is no coalescing, so the
/// two one-page pieces this leaves behind are never merged back into a
/// two-page block, and a subsequent two-page request has to take fresh bump
/// room even though two adjacent free pages exist. The test exists so that
/// the limit is a pinned, visible fact rather than a sentence in a doc
/// comment that nobody can check.
#[test]
fn freed_neighbours_are_not_coalesced() {
    if JIT_CODE_ARENA_IS_INERT {
        return;
    }
    let mut arena = test_arena();
    let page = code_page_size();

    let two = arena.alloc(page * 2).expect("two-page alloc");
    let two_addr = two.as_ptr() as usize;
    arena.free(two);
    // Split it into two adjacent one-page blocks, then free both.
    let lo = arena.alloc(page).expect("lo");
    let hi = arena.alloc(page).expect("hi");
    assert_eq!(lo.as_ptr() as usize, two_addr);
    assert_eq!(hi.as_ptr() as usize, two_addr + page);
    arena.free(lo);
    arena.free(hi);

    let census = arena.census();
    assert_eq!(census.free_blocks, 2, "two adjacent one-page blocks");
    assert_eq!(census.bytes_free_listed, page * 2);

    let again = arena.alloc(page * 2).expect("two-page alloc again");
    assert_ne!(
        again.as_ptr() as usize,
        two_addr,
        "a two-page request was served from two adjacent one-page free \
         blocks — this arena does not coalesce, so if that ever becomes \
         true the merge has to be reviewed for the W^X argument"
    );
    let census = arena.census();
    assert_eq!(
        census.bytes_free_listed,
        page * 2,
        "the two one-page blocks are still stranded: {census:?}"
    );
    assert_eq!(
        census.served_from_bump, 2,
        "the second two-page request had to take fresh bump room: {census:?}"
    );
    arena.free(again);
}

// -- the pressure report and the eviction seam -------------------------

/// The pressure report and the census must not be able to disagree.
///
/// They are computed by different code over the same state — the census is
/// a full O(live blocks) partition meant for a report point, the pressure
/// report is a cheap subset read on the allocation path — and the whole
/// value of the cheap one is that a policy can trust it. A mixed sequence
/// of allocations, frees, a split and a fallback is run first so that the
/// comparison is over a state with something in every bucket.
#[test]
fn the_pressure_report_agrees_with_the_census() {
    if JIT_CODE_ARENA_IS_INERT {
        return;
    }
    let mut arena = test_arena();
    let page = code_page_size();

    let a = arena.alloc(page * 3).expect("a");
    let b = arena.alloc(page).expect("b");
    arena.free(a); // three pages to the free list
    let c = arena.alloc(page).expect("c"); // splits a's block
    let d = arena.alloc(page * 5).expect("d"); // forces a second region
    assert_eq!(arena.census().blocks_split, 1);
    assert_eq!(arena.census().regions, 2);

    let census = arena.census();
    let pressure = arena.pressure();
    assert_eq!(pressure.arena_id, census.arena_id);
    assert_eq!(pressure.bytes_reserved, census.bytes_mapped);
    assert_eq!(pressure.bytes_live, census.bytes_live);
    assert_eq!(pressure.bytes_free_listed, census.bytes_free_listed);
    assert_eq!(pressure.regions, census.regions);
    assert_eq!(pressure.region_bytes, census.region_bytes);
    assert_eq!(pressure.live_blocks, census.live_blocks);
    assert!(
        pressure.last_alloc_mapped_region,
        "`d` did not fit either existing region and mapped one: {pressure:?}"
    );
    assert!(!pressure.last_alloc_was_standalone);

    // The cumulative counters are over every allocation, not over the live
    // ones, so they are checked against the requests this test actually
    // made rather than against the partition.
    assert_eq!(
        census.bytes_requested_total as usize,
        page * 3 + page + page + page * 5,
        "five requests, all of them exact page multiples: {census:?}"
    );
    assert_eq!(
        census.bytes_served_total, census.bytes_requested_total,
        "page-multiple requests cost no rounding: {census:?}"
    );
    assert_eq!(census.bytes_fragmentation_total, 0);

    // Now a request that is NOT a page multiple, so the fragmentation
    // counter has something to count.
    let e = arena.alloc(page + 1).expect("e");
    let census = arena.census();
    assert_eq!(
        census.bytes_fragmentation_total as usize,
        page - 1,
        "a (page + 1)-byte request takes two pages: {census:?}"
    );
    assert_eq!(
        census.bytes_fragmentation_total,
        census.bytes_served_total - census.bytes_requested_total
    );

    // And a standalone fallback, which the pressure report must report as
    // such: the arena declined to serve it, which is the condition an
    // eviction policy most wants to know about.
    let big = arena.alloc(arena.region_bytes() + 1).expect("standalone");
    let pressure = arena.pressure();
    assert!(pressure.last_alloc_was_standalone);
    assert!(!pressure.last_alloc_mapped_region);
    assert_eq!(
        pressure.bytes_reserved,
        arena.census().bytes_mapped,
        "a standalone mapping is not a region and must not be counted as \
         reserved arena space"
    );

    for block in [b, c, d, e, big] {
        arena.free(block);
    }
}

/// With no hook installed the arena never asks anyone anything.
///
/// The point of the assertion is the DEFAULT, not the counter: a seam whose
/// presence changed behaviour would not be a seam. An arena that maps three
/// regions and falls back once must record zero eviction requests.
#[test]
fn the_eviction_hook_is_not_called_when_unset() {
    if JIT_CODE_ARENA_IS_INERT {
        return;
    }
    let mut arena = test_arena();
    let page = code_page_size();
    assert!(!arena.has_eviction_hook());

    let blocks: Vec<ArenaBlock> = (0..17).map(|_| arena.alloc(page).expect("alloc")).collect();
    let over = arena.alloc(arena.region_bytes() + 1).expect("standalone");
    let census = arena.census();
    assert!(census.regions >= 3, "the run must have grown: {census:?}");
    assert_eq!(census.served_standalone, 1);
    assert_eq!(
        census.evictions_requested, 0,
        "an arena with no hook must never ask: {census:?}"
    );
    assert_eq!(census.evictions_performed, 0);

    for b in blocks {
        arena.free(b);
    }
    arena.free(over);
}

/// The hook is called exactly once per region the arena is about to create,
/// and not at all for an allocation the arena can already serve.
///
/// "Before a new region is created" is the contract, and the test checks it
/// from both sides: the count goes up in lockstep with `regions`, and an
/// allocation served from the bump cursor or from a free list leaves it
/// alone. The hook itself only touches an atomic — a hook that reached back
/// into this arena would deadlock on the mutex `alloc` holds, which is the
/// limitation [`JitCodeArenaEvictionHook`] documents.
#[test]
fn the_eviction_hook_is_called_once_before_each_new_region() {
    if JIT_CODE_ARENA_IS_INERT {
        return;
    }
    let mut arena = test_arena();
    let page = code_page_size();

    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen_regions = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX));
    {
        let calls = std::sync::Arc::clone(&calls);
        let seen_regions = std::sync::Arc::clone(&seen_regions);
        let previous =
            arena.set_eviction_hook(Box::new(move |pressure: &JitCodeArenaPressure| {
                calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                seen_regions.store(pressure.regions, std::sync::atomic::Ordering::Relaxed);
                // "I evicted nothing", which is the only honest answer a
                // hook that cannot free into a locked arena can give.
                false
            }));
        assert!(previous.is_none());
    }
    assert!(arena.has_eviction_hook());

    // First allocation: nothing to serve it from, so the arena is about to
    // map its first region and asks first.
    let first = arena.alloc(page).expect("first");
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(
        seen_regions.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "the hook is called BEFORE the region exists, so it must see zero"
    );
    assert_eq!(arena.census().regions, 1);

    // The next seven fit the region that was just mapped, so nobody is
    // asked anything.
    let rest: Vec<ArenaBlock> = (0..7).map(|_| arena.alloc(page).expect("rest")).collect();
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "an allocation the arena can already serve must not ask"
    );
    assert_eq!(arena.census().regions, 1);

    // The ninth does not fit, so the arena asks a second time and then maps
    // a second region.
    let ninth = arena.alloc(page).expect("ninth");
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    assert_eq!(
        seen_regions.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "the second call must see the one region that already exists"
    );
    assert_eq!(arena.census().regions, 2);

    let census = arena.census();
    assert_eq!(census.evictions_requested, 2);
    assert_eq!(
        census.evictions_performed, 0,
        "the hook answered `false` every time: {census:?}"
    );

    // A recycled allocation does not ask either: the free list is checked
    // before the seam fires.
    arena.free(first);
    let recycled = arena.alloc(page).expect("recycled");
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "an allocation served from a free list must not ask"
    );

    // Removing the hook restores the default, and the counters keep what
    // they measured rather than resetting.
    assert!(arena.take_eviction_hook().is_some());
    assert!(!arena.has_eviction_hook());
    let after = arena.alloc(page * 8).expect("after");
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "a removed hook must not be called"
    );
    assert_eq!(arena.census().evictions_requested, 2);

    for b in rest {
        arena.free(b);
    }
    arena.free(ninth);
    arena.free(recycled);
    arena.free(after);
}

/// A hook that claims to have evicted something is counted as having done
/// so, and the arena still gets its region.
///
/// This pins the honest reading of `evictions_performed`: it counts what
/// the OWNER said, not what the arena observed. A hook that returns `true`
/// without actually putting anything back on a free list — which is all a
/// hook can do while the arena lock is held — still ends with a region
/// mapped, and the census says so. Anyone using these two numbers to argue
/// that the seam is paying for itself has to compare
/// `evictions_performed` against `regions`, not read it on its own.
#[test]
fn an_eviction_that_frees_nothing_still_maps_a_region() {
    if JIT_CODE_ARENA_IS_INERT {
        return;
    }
    let mut arena = test_arena();
    let page = code_page_size();
    assert!(arena
        .set_eviction_hook(Box::new(|_: &JitCodeArenaPressure| true))
        .is_none());

    let block = arena.alloc(page).expect("alloc");
    let census = arena.census();
    assert_eq!(census.evictions_requested, 1);
    assert_eq!(
        census.evictions_performed, 1,
        "the hook said it evicted, so the arena records that it did: \
         {census:?}"
    );
    assert_eq!(
        census.regions, 1,
        "nothing was actually freed, so the region was mapped anyway: \
         {census:?}"
    );
    arena.free(block);
}

/// Two neighbouring arena blocks are on DISTINCT pages, so a protection
/// flip on one cannot reach the other.
///
/// The address comparison is the checkable half, and it is the half the
/// W^X argument is actually made of: `VirtualProtect` / `mprotect` round to
/// page boundaries, so "different pages" is exactly "cannot be reached".
/// The flip that follows exercises the real calls and then writes one byte
/// into the neighbour; that write is safe ONLY because the assertion above
/// it already established the two are not on one page, so a regression
/// shows up here as an access violation rather than as a failed assertion.
/// That asymmetry is why the assertion comes first and is not the
/// afterthought.
#[test]
fn make_executable_on_one_arena_block_cannot_reach_its_neighbour() {
    if JIT_CODE_ARENA_IS_INERT {
        // There is no per-page protection on macOS/ARM64 at all — W^X is
        // the thread-wide `pthread_jit_write_protect_np` toggle — and the
        // arena is inert there anyway.
        return;
    }
    let mut arena = test_arena();
    let page = code_page_size();
    let first = arena.alloc(8).expect("first block");
    let second = arena.alloc(8).expect("second block");

    let first_last_page = (first.as_ptr() as usize + first.size() - 1) / page;
    let second_first_page = second.as_ptr() as usize / page;
    assert!(
        first_last_page < second_first_page,
        "block {:#x}+{} ends on page {first_last_page} and block {:#x} starts \
         on page {second_first_page}; they share a page, so make_executable \
         on the first would flip the second",
        first.as_ptr() as usize,
        first.size(),
        second.as_ptr() as usize,
    );

    make_executable(first.as_ptr(), first.size()).expect("make_executable");
    {
        let _write = JitWriteScope::enter();
        // SAFETY: `second` is a live block this arena handed out, one byte
        // is inside it, and the assertion above proves it is not on a page
        // the `make_executable` just flipped.
        unsafe { *second.as_ptr() = 0x90 };
    }
    make_writable(first.as_ptr(), first.size()).expect("make_writable");

    arena.free(first);
    arena.free(second);
}

/// Cells must be distinct, cache-line aligned, and zero.
///
/// Zero is the part with teeth: `CacheLineFlag::new` writes the initial
/// value itself, but every other prospective caller (a counter, an epoch)
/// will read the cell before writing it, and "fresh anonymous pages are
/// zero-filled" is a property of the FIRST cell of a chunk, not of the
/// 900th.
#[test]
fn code_adjacent_cells_are_distinct_aligned_and_zero() {
    if near_globals_engaged() {
        // The allocator declines by design here; there are no cells to
        // assert about. See `alloc_code_adjacent_cell`.
        return;
    }
    let mut seen = std::collections::HashSet::new();
    for _ in 0..8 {
        let cell = alloc_code_adjacent_cell().expect("cell allocation failed");
        let addr = cell as usize;
        assert_eq!(
            addr % CODE_ADJACENT_CELL_SIZE,
            0,
            "cell {addr:#x} is not cache-line aligned, so it can share a \
             line with its neighbour"
        );
        assert!(seen.insert(addr), "cell {addr:#x} was handed out twice");
        // SAFETY: a freshly carved cell of exactly this size that nothing
        // else holds a reference to.
        let bytes = unsafe { std::slice::from_raw_parts(cell, CODE_ADJACENT_CELL_SIZE) };
        assert!(
            bytes.iter().all(|&b| b == 0),
            "cell {addr:#x} is not zeroed"
        );
        // Dirty it, so the next iteration's zero check is about the
        // allocator rather than about untouched pages.
        // SAFETY: as above; this cell is ours for the rest of the process.
        unsafe { std::ptr::write_bytes(cell, 0xA5, CODE_ADJACENT_CELL_SIZE) };
    }
}

/// The whole point: a cell must land within `disp32` reach of JIT code.
///
/// This is the property `jit/src/x64/safepoint.rs::emit_safepoint_poll`
/// silently loses when it does not hold — the poll stays correct and grows
/// from 7 bytes to 15 plus a clobbered register, at every loop back edge
/// and method entry in the process. Nothing else in the tree fails, which
/// is exactly why it went unnoticed from 2026-09-02 to 2026-09-10 with the
/// flag on the mimalloc heap, ~124 TB from the code cache.
///
/// x86-64 only: `disp32` reach is what makes the distance matter, and
/// aarch64's `ADRP` has its own (±4 GB) window that no emitter uses yet.
///
/// Both sides come from `platform_alloc`, so this asserts that the OS puts
/// two allocations from one primitive in one region. That is true of every
/// mapper we run on but is not architecturally guaranteed; if it ever
/// fails, the fallback still runs and this test is the notification.
#[cfg(target_arch = "x86_64")]
#[test]
fn a_cell_is_within_disp32_of_a_code_buffer() {
    if near_globals_engaged() {
        // `near_globals` owns placement and this allocator declines, so
        // there is no cell whose distance means anything. Reach in that
        // configuration is that strategy's own claim, asserted by
        // `near_globals_tests`.
        return;
    }
    let code = alloc_executable(4096).expect("alloc_executable failed");
    let cell = alloc_code_adjacent_cell().expect("cell allocation failed");
    // Widening: usize -> i128, so the subtraction cannot wrap.
    let delta = (cell as usize as i128) - (code as usize as i128);
    let in_reach = delta >= i32::MIN as i128 && delta <= i32::MAX as i128;
    free_executable(code, 4096);
    assert!(
        in_reach,
        "code buffer {:#x} and data cell {:#x} are {:.1} GB apart; a \
         RIP-relative poll cannot reach, so every safepoint poll in the \
         process falls back to MOV r64, imm64",
        code as usize,
        cell as usize,
        (delta.unsigned_abs() as f64) / (1024.0 * 1024.0 * 1024.0),
    );
}

#[test]
fn alloc_write_free() {
    let size = 4096;
    let ptr = alloc_executable(size).expect("alloc_executable failed");
    assert!(!ptr.is_null());

    // Write a pattern (memory starts as RW; on macOS/ARM64 the scope is
    // what makes it writable for this thread).
    {
        let _write = JitWriteScope::enter();
        // SAFETY: `ptr` is a fresh `size`-byte allocation that nothing
        // else references, and every index is below `size`.
        unsafe {
            for i in 0..size {
                *ptr.add(i) = (i & 0xFF) as u8;
            }
        }
    }

    // Finalize: transition from RW to RX
    make_executable(ptr, size).expect("make_executable failed");

    // Read back (reading is allowed in both RW and RX)
    // SAFETY: as above; reads stay in bounds and the region is readable.
    unsafe {
        for i in 0..size {
            assert_eq!(*ptr.add(i), (i & 0xFF) as u8);
        }
    }

    free_executable(ptr, size);
}

/// `MAP_ANONYMOUS` differs per OS, and the table must say so on EVERY host.
///
/// The Unix allocator used Linux's `0x20` for Linux, FreeBSD and macOS
/// x86-64 alike. On the BSDs and Darwin that is not `MAP_ANON`, `mmap`
/// failed with `EINVAL`, and the JIT was silently dead on those systems.
/// This checks the table itself, so a Windows or Linux CI run catches a
/// regression for a platform it cannot execute.
#[test]
fn map_anonymous_follows_each_os_abi() {
    assert_eq!(
        map_anonymous(UnixMmapAbi::Linux),
        0x20,
        "linux <asm-generic/mman-common.h>"
    );
    assert_eq!(
        map_anonymous(UnixMmapAbi::LinuxMips),
        0x800,
        "linux <asm/mman.h> on MIPS"
    );
    assert_eq!(
        map_anonymous(UnixMmapAbi::Bsd),
        0x1000,
        "Darwin and FreeBSD <sys/mman.h>"
    );
    assert_eq!(
        map_anonymous(UnixMmapAbi::Solaris),
        0x100,
        "illumos <sys/mman.h>"
    );
    assert_ne!(
        map_anonymous(UnixMmapAbi::Linux),
        map_anonymous(UnixMmapAbi::Bsd),
        "the two families genuinely differ -- one constant cannot serve both"
    );
}

/// The host picks the family its own headers use.
#[test]
fn the_host_mmap_abi_is_this_targets_own() {
    #[cfg(all(target_os = "linux", not(target_arch = "mips64")))]
    assert_eq!(HOST_MMAP_ABI, Some(UnixMmapAbi::Linux));
    #[cfg(any(target_os = "macos", target_os = "freebsd"))]
    assert_eq!(HOST_MMAP_ABI, Some(UnixMmapAbi::Bsd));
    #[cfg(target_os = "windows")]
    assert_eq!(HOST_MMAP_ABI, None, "Windows never maps through mmap");
}

/// A failed `mprotect` reports `errno`, not its `-1` return value.
///
/// A pointer one byte into a page is not page-aligned, which POSIX makes
/// `EINVAL` (22 on Linux, the BSDs and Darwin alike). Asked of the OS arm
/// (`platform_make_executable`): since JIT r23 wave 8 the public
/// [`make_executable`] rounds its range out to whole pages ([`page_span`]),
/// so the same call through it succeeds (asserted at the end).
#[cfg(all(
    not(target_os = "windows"),
    not(all(target_os = "macos", target_arch = "aarch64"))
))]
#[test]
fn a_protect_failure_carries_errno() {
    let size = 4096;
    let ptr = alloc_executable(size).expect("alloc_executable failed");
    // SAFETY: one byte into a 4096-byte allocation is in bounds.
    let misaligned = unsafe { ptr.add(1) };
    match platform_make_executable(misaligned, size - 1) {
        Err(JitError::ProtectFailed(code)) => {
            assert_ne!(
                code, -1,
                "the payload must be errno, not mprotect's return value"
            );
            assert_eq!(code, 22, "a misaligned mprotect is EINVAL");
        }
        other => panic!("a misaligned mprotect must fail, got {other:?}"),
    }
    assert!(
        make_executable(misaligned, size - 1).is_ok(),
        "the public call protects the whole page holding an unaligned range"
    );
    free_executable(ptr, size);
}

/// JIT r23 wave 8 (lane L5): [`page_span`] rounds the start down to its
/// page and grows the length by exactly that much; an aligned range is
/// unchanged.
#[test]
fn r23_w8_l5_page_span_covers_the_range_from_its_page() {
    let page = code_page_size();
    let base = (16 * page) as *mut u8;
    assert_eq!(page_span(base, 100), (base, 100));
    let inner = (16 * page + 0x7c0) as *mut u8;
    assert_eq!(page_span(inner, 100), (base, 100 + 0x7c0));
    let last = (17 * page - 1) as *mut u8;
    assert_eq!(page_span(last, 1), (base, page));
}

/// Scopes nest, and a write inside the innermost one lands.
#[test]
fn jit_write_scopes_nest() {
    let size = 4096;
    let ptr = alloc_executable(size).expect("alloc_executable failed");
    {
        let _outer = JitWriteScope::enter();
        {
            let _inner = JitWriteScope::enter();
            // SAFETY: in bounds of a fresh allocation nothing else holds.
            unsafe { *ptr = 0x5A };
        }
        // Still inside the outer scope: the inner drop must not have
        // re-protected the page for this thread.
        // SAFETY: as above.
        unsafe { *ptr.add(1) = 0xA5 };
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert_eq!(JIT_WRITE_DEPTH.with(|d| d.get()), 1);
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    assert_eq!(
        JIT_WRITE_DEPTH.with(|d| d.get()),
        0,
        "the thread is back in execute mode"
    );
    make_executable(ptr, size).expect("make_executable failed");
    // SAFETY: reads in bounds of a readable region.
    unsafe {
        assert_eq!(*ptr, 0x5A);
        assert_eq!(*ptr.add(1), 0xA5);
    }
    free_executable(ptr, size);
}

/// A zero-length request must be REFUSED, on every platform we build for.
///
/// The old name (`alloc_zero_returns_some`) was the lie: no backend can
/// satisfy it. `VirtualAlloc(NULL, 0, ...)` fails with
/// `ERROR_INVALID_PARAMETER` (87), and POSIX specifies `mmap` with
/// `len == 0` fails with `EINVAL` — so both `platform_alloc` bodies take
/// their null / `MAP_FAILED` arm and answer `None`. The failure is that a
/// caller ever gets a `Some` it would then write code into and hand to
/// `make_executable` on a zero-byte region.
///
/// Was vacuous: `if let Some(ptr) = alloc_executable(0) { free… }` — the
/// interesting branch was the silent one, so deleting the `p.is_null()` /
/// `p == MAP_FAILED` check in `platform_alloc` (returning `Some(null)`)
/// stayed green.
#[test]
fn alloc_zero_is_refused() {
    assert!(
        alloc_executable(0).is_none(),
        "a zero-length JIT allocation must be refused, not handed back"
    );
}

#[test]
fn make_executable_then_writable_roundtrip() {
    let size = 4096;
    let ptr = alloc_executable(size).expect("alloc_executable failed");

    // Write initial data
    {
        let _write = JitWriteScope::enter();
        // SAFETY: in bounds of a fresh allocation nothing else references.
        unsafe {
            *ptr = 0xAA;
        }
    }

    // Transition RW -> RX
    make_executable(ptr, size).expect("make_executable failed");

    // Read should still work
    // SAFETY: an in-bounds read of a readable region.
    unsafe {
        assert_eq!(*ptr, 0xAA);
    }

    // Transition RX -> RW for patching
    make_writable(ptr, size).expect("make_writable failed");

    // Write new data
    {
        let _write = JitWriteScope::enter();
        // SAFETY: as above; the region is writable again.
        unsafe {
            *ptr = 0xBB;
        }
    }

    // Transition back to RX
    make_executable(ptr, size).expect("make_executable failed");
    // SAFETY: an in-bounds read of a readable region.
    unsafe {
        assert_eq!(*ptr, 0xBB);
    }

    free_executable(ptr, size);
}

/// `Display` must carry the OS code, which is the only thing in the error
/// that a log reader can act on.
///
/// `-1` is asserted because that is what `mprotect` returns for every
/// failure; the payload is deliberately the `errno`, and a `Display` that
/// dropped it would leave the operator with "protect failed" and nothing
/// else. The `AllocationFailed` half of this test went with the variant in
/// round 10 wave 8.
#[test]
fn jit_error_display() {
    let e = JitError::ProtectFailed(-1);
    assert!(format!("{}", e).contains("-1"));
    assert!(format!("{}", JitError::ProtectFailed(13)).contains("13"));
}

/// `flush_icache_range_windows` must be callable on any Windows host.
///
/// The production call site is arch-gated (x86/x86-64 have coherent
/// caches and skip it), so on an ordinary x86-64 Windows CI machine the
/// call site never runs and a broken `extern` declaration — wrong
/// calling convention, wrong argument widths, missing symbol — would
/// only surface on a Windows-on-ARM build. Calling it directly here
/// forces the declaration to be compiled, linked and executed on every
/// Windows host. `FlushInstructionCache` is valid (and a documented
/// no-op) on x86-64, so this is safe to run anywhere Windows runs.
#[cfg(target_os = "windows")]
#[test]
fn windows_icache_flush_is_callable() {
    let size = 4096;
    let ptr = alloc_executable(size).expect("alloc_executable failed");
    // SAFETY: in bounds of a fresh RW allocation nothing else references.
    unsafe {
        *ptr = 0x90; // one byte of "code" so the range is not untouched
    }
    flush_icache_range_windows(ptr, size);
    // Range covering a sub-page slice, and a zero-length range: both are
    // legal arguments and must not fault.
    flush_icache_range_windows(ptr, 4);
    flush_icache_range_windows(ptr, 0);
    free_executable(ptr, size);
}

/// `flush_icache_range_unix` must be callable on any Unix host.
///
/// The mirror of `windows_icache_flush_is_callable`. On x86-64 Unix — the
/// machine CI actually runs — this resolves to the documented no-op body,
/// which is exactly the point: the two bodies must keep the SAME
/// signature, so the unconditional call in `platform_make_executable`
/// compiles on every target. When the allow-list version of this gate was
/// in place there was no such coupling, and `aarch64-linux-android` simply
/// got no cache maintenance at all.
#[cfg(not(any(
    target_os = "windows",
    all(target_os = "macos", target_arch = "aarch64")
)))]
#[test]
fn unix_icache_flush_is_callable_on_every_target() {
    let size = 4096;
    let ptr = alloc_executable(size).expect("alloc_executable failed");
    // SAFETY: in bounds of a fresh RW allocation nothing else references.
    unsafe {
        *ptr = 0x90; // one byte of "code" so the range is not untouched
    }
    // SAFETY: `ptr..ptr+size` is one allocation, so every range below is
    // within it or one past its end — the helper's whole contract.
    unsafe {
        flush_icache_range_unix(ptr, size);
        // A sub-page slice and a zero-length range: both legal.
        flush_icache_range_unix(ptr, 4);
        flush_icache_range_unix(ptr, 0);
    }
    free_executable(ptr, size);
}

/// The reader-side barrier must be callable, and cheap, on every host.
///
/// It compiles to a single `ISB` on aarch64 and to nothing on x86-64, so
/// the only thing that can break is the inline-asm constraint set — which
/// this catches on any ARM host and which the x86 build cannot catch at
/// all. Running it a few times also documents that it is idempotent: a
/// caller that is unsure whether it is the publishing thread should just
/// call it.
#[test]
fn the_reader_side_instruction_stream_barrier_is_callable() {
    for _ in 0..4 {
        synchronize_instruction_stream();
    }
}

/// The epoch gate issues one barrier per thread per publication epoch, and
/// none at all in between.
///
/// This is the property that makes the barrier affordable on the dispatch
/// path: the ARM requirement is per newly-published instruction stream, not
/// per call, so a dispatcher that issued an `ISB` on every compiled
/// invocation would be paying a pipeline flush for a publication that
/// happened once an hour ago.
///
/// Run on its own thread because the gate's state is thread-local and the
/// seed ("this thread has never synchronised") is only observable once. The
/// counter is process-wide, so this is also the only test in the file that
/// may touch it.
///
/// On x86-64 the whole mechanism is compiled out, and since round 10 wave 8
/// the accessor says so: every reading is `None`, not `Some(0)`.
///
/// That change matters to this test more than to any caller. The previous
/// x86-64 arm asserted `(first - start, repeats - first, publication -
/// repeats) == (0, 0, 0)` on a `u64`, and three zero deltas are what a
/// compiled-out gate produces *and* what a gate that ran but never
/// incremented produces *and* what a gate whose counter was removed
/// produces. It could not fail. `None` on all four readings is a statement
/// about the target rather than about a delta, and it does fail if the
/// accessor ever starts reporting a count on an architecture that cannot
/// have one.
#[test]
fn the_epoch_gated_barrier_runs_once_per_publication_epoch() {
    let readings = std::thread::spawn(|| {
        let start = instruction_stream_barriers_issued();
        // Epoch 0 is a legal epoch, and a thread that has never
        // synchronised must not mistake itself for one that already has at
        // epoch 0 — which is why the seed is `u64::MAX`.
        synchronize_instruction_stream_for_epoch(0);
        let after_first = instruction_stream_barriers_issued();
        for _ in 0..8 {
            synchronize_instruction_stream_for_epoch(0);
        }
        let after_repeats = instruction_stream_barriers_issued();
        synchronize_instruction_stream_for_epoch(1);
        let after_publication = instruction_stream_barriers_issued();
        (start, after_first, after_repeats, after_publication)
    })
    .join()
    .expect("the gate must not panic");

    let (start, first, repeats, publication) = readings;
    #[cfg(not(target_arch = "x86_64"))]
    {
        // `expect` rather than `unwrap_or(0)`: a `None` here would mean the
        // accessor has decided this target has no barrier mechanism while
        // the mechanism is compiled in, and folding that into a zero is how
        // the delta assertions below would pass on a broken accessor.
        let start = start.expect("a non-x86-64 target counts its barriers");
        let first = first.expect("a non-x86-64 target counts its barriers");
        let repeats = repeats.expect("a non-x86-64 target counts its barriers");
        let publication = publication.expect("a non-x86-64 target counts its barriers");
        assert_eq!(first - start, 1, "a thread's first dispatch synchronises");
        assert_eq!(repeats - first, 0, "eight more calls at the same epoch");
        assert_eq!(publication - repeats, 1, "a new epoch synchronises again");
    }
    #[cfg(target_arch = "x86_64")]
    {
        assert_eq!(
            (start, first, repeats, publication),
            (None, None, None, None),
            "x86-64 has no reader-side barrier to count, which is a different \
             statement from having counted zero of them — and unlike three \
             zero deltas, this one can fail"
        );
    }
}

/// End-to-end check that `make_executable` flushes the icache on
/// aarch64 so a freshly-written function actually executes.
///
/// We allocate a page, write a single `RET` instruction (0xD65F03C0
/// little-endian on aarch64 = `ret`), finalize via `make_executable`
/// (which on every non-macOS aarch64 Unix also runs `__clear_cache`), then
/// transmute the pointer to a `fn()` and call it. On x86-64 hosts
/// this test is compiled out; on macOS aarch64 it exercises the
/// existing `sys_icache_invalidate` path, which is a useful
/// regression guard too.
///
/// The cfg is "aarch64 and not Windows" rather than a list of operating
/// systems for the same reason the production gate is: an allow-list
/// omitted `aarch64-linux-android`, so the one target that had no flush
/// also had no test that would have noticed.
#[cfg(all(target_arch = "aarch64", not(target_os = "windows")))]
#[test]
fn aarch64_icache_flush_executes_fresh_code() {
    let size = 4096;
    let ptr = alloc_executable(size).expect("alloc_executable failed");

    // aarch64 `ret` = 0xD65F03C0, little-endian byte sequence
    // 0xC0 0x03 0x5F 0xD6.
    {
        let _write = JitWriteScope::enter();
        // SAFETY: four in-bounds bytes of a fresh allocation.
        unsafe {
            *ptr.add(0) = 0xC0;
            *ptr.add(1) = 0x03;
            *ptr.add(2) = 0x5F;
            *ptr.add(3) = 0xD6;
        }
    }

    make_executable(ptr, size).expect("make_executable failed");

    // Jump to the freshly-written code. If the icache was not
    // flushed the CPU may fetch zero bytes (UDF) or stale data
    // and trap; a clean flush makes this a no-op call that
    // returns normally.
    // SAFETY: the region now holds exactly one `RET` and is executable, so
    // it is a valid `extern "C" fn()` that returns immediately.
    let f: extern "C" fn() = unsafe { std::mem::transmute(ptr) };
    f();

    free_executable(ptr, size);
}
