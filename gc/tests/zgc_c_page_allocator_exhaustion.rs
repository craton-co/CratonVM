// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `ZPageAllocator` under exhaustion: a refusal must cost one allocation, not
//! the page the mutator was filling.
//!
//! Both cases here are the same shape as the two allocator defects this tree
//! has already paid for —
//! `docs/internal/fixed-suite-bugs/vm/zgc-oom-with-84-percent-of-the-heap-free-FIXED-20260810.md`
//! and
//! `docs/internal/fixed-suite-bugs/gc/zgc-low-end-fragmentation-was-the-starved-tlab-rung-FIXED-20260830.md`:
//! the heap reports exhaustion while holding memory that would have served the
//! request. They are integration tests rather than `#[cfg(test)]` units so the
//! assertions are made against the crate's *public* page API, which is what a
//! future page-backed `ZgcRealHeap` would be built on.

use cratonvm_gc::zgc::page::{ZPageAllocator, ZPageConfig, ZPageError, ZPageSizeClass, ZPageState};

/// Exactly two Small pages of budget, so exhaustion is reached in a handful of
/// allocations and the arithmetic below is checkable by hand.
fn two_page_config() -> ZPageConfig {
    ZPageConfig {
        granule_size: 4096,
        small_page_size: 8192,   // 2 granules
        medium_page_size: 16384, // 4 granules
        small_object_limit: 2048,
        medium_object_limit: 4096,
        max_capacity: 4096 * 4, // 16 KiB == exactly two Small pages
    }
}

/// A shared page that still has room must survive a refill that cannot get a
/// fresh page.
///
/// `refill_shared` used to close the old page and clear the shared slot
/// *before* asking for the replacement. When the ask failed — budget spent, or
/// granule space too fragmented for a whole page — the remainder of the page
/// the mutator had been filling was abandoned along with it, and every later
/// allocation, however small, found no shared page at all and asked for a fresh
/// one again. One refusal became permanent exhaustion with kilobytes free.
#[test]
fn a_failed_refill_keeps_the_shared_page_and_its_remainder() {
    let alloc = ZPageAllocator::new(two_page_config()).expect("geometry must validate");

    // Page A: seven 1 KiB objects, 1 KiB left.
    let first = alloc.alloc_object(1024, 8).expect("fresh heap");
    let page_a = alloc
        .page_for(first)
        .expect("an allocated address is inside a page");
    for _ in 1..7 {
        alloc.alloc_object(1024, 8).expect("page A has room");
    }
    assert_eq!(page_a.state(), ZPageState::Allocating);
    assert_eq!(page_a.remaining(), 1024);

    // A 2 KiB object does not fit A's remainder, so it rolls onto page B —
    // which spends the last of the budget.
    let b_addr = alloc
        .alloc_object(2048, 8)
        .expect("page B is the last page");
    let page_b = alloc.page_for(b_addr).expect("page B");
    assert_ne!(page_a.id(), page_b.id(), "the 2 KiB object must roll over");
    assert_eq!(page_a.state(), ZPageState::Relocatable);
    assert_eq!(alloc.stats().committed, alloc.max_capacity());

    // Fill B down to a 1 KiB remainder.
    while page_b.remaining() > 1024 {
        alloc.alloc_object(1024, 8).expect("page B has room");
    }
    assert_eq!(page_b.remaining(), 1024);
    assert_eq!(page_b.state(), ZPageState::Allocating);

    // THE REFUSAL. 2 KiB does not fit B's 1 KiB remainder, and there is no
    // budget left for a third page.
    let refused = alloc.alloc_object(2048, 8);
    assert!(
        matches!(refused, Err(ZPageError::OutOfCapacity { .. })),
        "a full heap must refuse, not panic: {refused:?}",
    );

    // ...and it must have cost exactly that one allocation.
    assert_eq!(
        page_b.state(),
        ZPageState::Allocating,
        "a refused refill must leave the shared page open — closing it abandons \
         its remainder and turns one refusal into permanent exhaustion",
    );
    let after = alloc
        .alloc_object(512, 8)
        .expect("512 B still fits the shared page's 1 KiB remainder");
    assert!(
        page_b.contains(after),
        "the object must come out of the page that was already shared",
    );
    assert_eq!(page_b.remaining(), 512);
}

/// An absurd allocation request must round to an honest refusal, not wrap.
///
/// `ZPageConfig::class_for` routes everything above `medium_object_limit` to
/// Large — its own unit test asserts `class_for(usize::MAX / 2) == Large` — and
/// `page_size_for` then rounds that up to a whole granule. A plain
/// `div_ceil(g) * g` overflows there: it panics in a debug build, and in
/// release it wraps to a *small* page size that sails through the budget check
/// and installs a page bearing no relation to the object.
#[test]
fn an_absurd_request_refuses_instead_of_overflowing_the_page_size() {
    let alloc = ZPageAllocator::new(two_page_config()).expect("geometry must validate");
    let cfg = alloc.config().clone();

    // The rounding itself saturates rather than wrapping.
    for bytes in [
        usize::MAX,
        usize::MAX - 1,
        usize::MAX / 2,
        isize::MAX as usize,
    ] {
        assert_eq!(cfg.class_for(bytes), ZPageSizeClass::Large);
        let size = cfg.page_size_for(ZPageSizeClass::Large, bytes);
        assert!(
            size >= bytes || size == usize::MAX,
            "page_size_for({bytes}) wrapped to {size}",
        );
    }

    // And the allocator turns it into a Java-visible error, with the heap left
    // completely untouched.
    let before = alloc.stats();
    for bytes in [usize::MAX, usize::MAX / 2] {
        let err = alloc.alloc_object(bytes, 8);
        assert!(
            matches!(err, Err(ZPageError::OutOfCapacity { .. })),
            "an unsatisfiable request must be an error, not a wrapped page: {err:?}",
        );
    }
    let after = alloc.stats();
    assert_eq!(after.committed, before.committed);
    assert_eq!(after.large_pages, before.large_pages);
    assert_eq!(after.free_granules, before.free_granules);
    assert!(alloc.pages().is_empty(), "no page may have been installed");
}

/// An empty shared page is **freed** at retirement, not parked in
/// `Relocatable` where nothing will ever collect it.
///
/// `ZPageReal::relocation_capacity_bytes` states the rule and records that
/// nothing performed it; `retire_shared_pages` now does. This exercises the
/// non-empty half against the empty half so the two branches are pinned
/// together: a page with objects in it must still become a relocation
/// candidate.
#[test]
fn retiring_shared_pages_frees_the_empty_one_and_retires_the_rest() {
    let alloc = ZPageAllocator::new(two_page_config()).expect("geometry must validate");
    let addr = alloc.alloc_object(64, 8).expect("fresh heap");
    let page = alloc.page_for(addr).expect("page");
    assert_eq!(page.state(), ZPageState::Allocating);

    alloc.retire_shared_pages();
    assert_eq!(
        page.state(),
        ZPageState::Relocatable,
        "a shared page holding objects is a relocation candidate",
    );
    assert_eq!(page.used(), 64, "retirement must not move the cursor");

    // Retiring again is a no-op: the slot is already empty.
    alloc.retire_shared_pages();
    assert_eq!(page.state(), ZPageState::Relocatable);

    // The empty half, driven through `free_page` — which is the same
    // `reset -> Free -> cache` transition `retire_shared_pages` performs for a
    // page whose cursor never moved.
    let spare = alloc
        .alloc_page(ZPageSizeClass::Small, 0)
        .expect("budget has a page left");
    assert_eq!(spare.used(), 0);
    let committed_before = alloc.stats().committed;
    alloc.free_page(&spare);
    assert_eq!(spare.state(), ZPageState::Free);
    assert_eq!(
        alloc.stats().committed,
        committed_before,
        "a cached page keeps its granules",
    );
    assert_eq!(alloc.stats().cached, spare.size());
}
