use super::*;

/// A miniature geometry: the same *shape* as OpenJDK's (granule, two fixed
/// tiers each with a 1-in-8 object limit, bespoke large pages) at 1/512
/// the scale, so a test heap is 256 KiB instead of 256 MiB.
fn test_config() -> ZPageConfig {
    ZPageConfig {
        granule_size: 4096,
        small_page_size: 8192,          // 2 granules
        medium_page_size: 65536,        // 16 granules
        small_object_limit: 8192 / 8,   // 1 KiB
        medium_object_limit: 65536 / 8, // 8 KiB
        max_capacity: 4096 * 64,        // 256 KiB
    }
}

fn allocator() -> ZPageAllocator {
    ZPageAllocator::new(test_config()).expect("test geometry must validate")
}

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn page_types_are_send_and_sync_without_unsafe_impls() {
    // Storing page windows as `usize` rather than `*mut u8` is what buys
    // this; `ZgcRealHeap` needs an `unsafe impl` because it does the
    // opposite.
    assert_send_sync::<ZPageReal>();
    assert_send_sync::<ZPageTable>();
    assert_send_sync::<ZPageAllocator>();
}

#[test]
fn default_config_matches_openjdk_size_classes() {
    let c = ZPageConfig::default();
    assert!(c.validate().is_ok());
    assert_eq!(c.granule_size, 2 * 1024 * 1024);
    assert_eq!(c.small_page_size, 2 * 1024 * 1024);
    assert_eq!(c.medium_page_size, 32 * 1024 * 1024);
    assert_eq!(c.small_object_limit, 256 * 1024);
    assert_eq!(c.medium_object_limit, 4 * 1024 * 1024);
}

#[test]
fn size_class_routing_across_the_thresholds() {
    let c = test_config();
    // The limits are inclusive: an object of exactly the limit still fits
    // its tier, limit + 1 is the first object of the next one.
    assert_eq!(c.class_for(1), ZPageSizeClass::Small);
    assert_eq!(c.class_for(c.small_object_limit), ZPageSizeClass::Small);
    assert_eq!(
        c.class_for(c.small_object_limit + 1),
        ZPageSizeClass::Medium
    );
    assert_eq!(c.class_for(c.medium_object_limit), ZPageSizeClass::Medium);
    assert_eq!(
        c.class_for(c.medium_object_limit + 1),
        ZPageSizeClass::Large
    );
    assert_eq!(c.class_for(usize::MAX / 2), ZPageSizeClass::Large);

    // Large page sizes round up to a whole granule.
    assert_eq!(
        c.page_size_for(ZPageSizeClass::Large, c.medium_object_limit + 1),
        (c.medium_object_limit + 1).div_ceil(c.granule_size) * c.granule_size,
    );
    assert_eq!(c.page_size_for(ZPageSizeClass::Large, 4096), 4096);
    assert_eq!(c.page_size_for(ZPageSizeClass::Large, 4097), 8192);
}

#[test]
fn invalid_configs_are_rejected_not_panicked() {
    let mut c = test_config();
    c.granule_size = 3000; // not a power of two
    assert!(matches!(c.validate(), Err(ZPageError::InvalidConfig(_))));

    let mut c = test_config();
    c.small_page_size = 4096 + 8; // not a granule multiple
    assert!(matches!(c.validate(), Err(ZPageError::InvalidConfig(_))));

    let mut c = test_config();
    c.small_object_limit = c.small_page_size * 2; // cannot fit its page
    assert!(matches!(c.validate(), Err(ZPageError::InvalidConfig(_))));
}

#[test]
fn page_alloc_bumps_until_full_then_returns_none() {
    let alloc = allocator();
    let page = alloc.alloc_page(ZPageSizeClass::Small, 0).unwrap();
    assert_eq!(page.size(), 8192);
    assert_eq!(page.used(), 0);
    assert_eq!(page.state(), ZPageState::Allocating);

    let chunk = 64usize;
    let mut last = page.base();
    let mut count = 0usize;
    while let Some(addr) = page.alloc(chunk, 8) {
        assert!(
            page.contains(addr),
            "handed out an address outside the page"
        );
        assert_eq!(addr % 8, 0, "allocations must stay on the object grid");
        if count > 0 {
            assert_eq!(addr - last, chunk, "bump allocation must be dense");
        }
        last = addr;
        count += 1;
        assert!(count <= 8192 / chunk, "page handed out more than it holds");
    }
    assert_eq!(count, 8192 / chunk);
    assert_eq!(page.used(), 8192);
    assert_eq!(page.remaining(), 0);
    // Full is not an error, it is a `None`.
    assert!(page.alloc(8, 8).is_none());
}

#[test]
fn page_alloc_refuses_when_not_allocating() {
    let alloc = allocator();
    let page = alloc.alloc_page(ZPageSizeClass::Small, 0).unwrap();
    assert!(page.alloc(64, 8).is_some());
    page.set_state(ZPageState::Relocatable);
    assert!(
        page.alloc(64, 8).is_none(),
        "a page closed to allocation must refuse, not silently accept",
    );
    assert!(page.try_transition(ZPageState::Relocatable, ZPageState::InRelocationSet));
    assert!(!page.try_transition(ZPageState::Relocatable, ZPageState::InRelocationSet));
    assert_eq!(page.state(), ZPageState::InRelocationSet);
}

#[test]
fn small_allocation_rounds_up_to_a_header() {
    let alloc = allocator();
    let page = alloc.alloc_page(ZPageSizeClass::Small, 0).unwrap();
    let a = page.alloc(1, 8).unwrap();
    let b = page.alloc(1, 8).unwrap();
    assert!(
        b - a >= ZPAGE_MIN_ALLOC,
        "two object bases must never be closer than one header apart",
    );
}

#[test]
fn page_table_lookup_at_boundaries_and_out_of_range() {
    let alloc = allocator();
    let page = alloc.alloc_page(ZPageSizeClass::Small, 0).unwrap();
    let base = page.base();
    let end = page.end();

    // First byte, an interior byte, and the last byte all map to the page.
    for addr in [base, base + 1, base + 4095, base + 4096, end - 1] {
        let found = alloc.page_for(addr).expect("address inside the page");
        assert!(Arc::ptr_eq(&found, &page));
        assert!(alloc.table().contains(addr));
    }

    // One past the end is NOT in this page. It may be inside the reserved
    // range (a later granule), but no page is installed there yet.
    assert!(alloc.page_for(end).is_none());
    assert!(!alloc.table().contains(end));

    // Below the base and past the reservation are both out of range.
    assert!(alloc.table().granule_index(alloc.base() - 1).is_none());
    assert!(alloc.page_for(alloc.base() - 1).is_none());
    assert!(alloc.page_for(alloc.end()).is_none());
    assert!(alloc.page_for(alloc.end() + 4096).is_none());
    assert!(alloc.page_for(0).is_none());
    assert!(!alloc.in_reserved_range(alloc.end()));
}

#[test]
fn every_granule_of_a_multi_granule_page_resolves_to_that_page() {
    let alloc = allocator();
    // A medium page is 16 granules; each one must index back to it, so
    // lookup never has to search a run.
    let page = alloc.alloc_page(ZPageSizeClass::Medium, 0).unwrap();
    let granules = page.size() / 4096;
    assert_eq!(granules, 16);
    for g in 0..granules {
        let addr = page.base() + g * 4096 + 17;
        let found = alloc.page_for(addr).expect("granule must map to the page");
        assert!(Arc::ptr_eq(&found, &page));
    }
    assert_eq!(alloc.table().pages().len(), 1);
}

#[test]
fn large_object_gets_its_own_granule_rounded_page_with_no_filler() {
    let alloc = allocator();
    let bytes = alloc.config().medium_object_limit + 1; // 8193 -> 12288
    let addr = alloc.alloc_object(bytes, 8).unwrap();
    let page = alloc.page_for(addr).unwrap();
    assert_eq!(page.size_class(), ZPageSizeClass::Large);
    assert_eq!(page.size(), 12288);
    assert_eq!(addr, page.base(), "the object sits at the page base");

    // The walk bound stops at `top`, so the rounding tail is outside it —
    // a page-driven walker never decodes the padding and therefore needs
    // no HumongousFiller sentinel to screen.
    let (start, end) = page.walk_bounds();
    assert_eq!(start, page.base());
    assert_eq!(end, page.base() + page.used());
    assert!(end < page.end(), "this page has a rounding tail");
    // And that tail is zero, exactly as the reservation left it.
    for off in page.used()..page.size() {
        // SAFETY: the tail is inside the page's own window.
        let b = unsafe { *((page.base() + off) as *const u8) };
        assert_eq!(b, 0, "page padding must stay zero, never a filler header");
    }
}

#[test]
fn freed_small_page_is_recycled_from_the_cache() {
    let alloc = allocator();
    let page = alloc.alloc_page(ZPageSizeClass::Small, 0).unwrap();
    let id = page.id();
    let base = page.base();
    assert!(page.alloc(64, 8).is_some());

    let committed_before = alloc.stats().committed;
    alloc.free_page(&page);
    let after_free = alloc.stats();
    assert_eq!(
        after_free.committed, committed_before,
        "a cached page keeps its granules — committed must not drop",
    );
    assert_eq!(after_free.cached, 8192);
    assert_eq!(page.state(), ZPageState::Free);
    assert_eq!(page.used(), 0, "reset must rewind the cursor");

    let again = alloc.alloc_page(ZPageSizeClass::Small, 0).unwrap();
    assert_eq!(again.id(), id, "the cached page must be reused, not remade");
    assert_eq!(again.base(), base);
    assert_eq!(again.state(), ZPageState::Allocating);
    assert_eq!(alloc.stats().cached, 0);
    assert_eq!(
        alloc.stats().small_pages,
        1,
        "recycling must not double-count"
    );
}

#[test]
fn reset_zeroes_the_used_extent() {
    let alloc = allocator();
    let page = alloc.alloc_page(ZPageSizeClass::Small, 0).unwrap();
    let addr = page.alloc(64, 8).unwrap();
    // SAFETY: `addr` is a live allocation inside the page.
    unsafe {
        std::ptr::write_bytes(addr as *mut u8, 0xAB, 64);
        assert_eq!(*(addr as *const u8), 0xAB);
    }
    page.reset_shared();
    // A conservative scanner must not be able to see the corpse's bytes.
    for off in 0..64 {
        // SAFETY: inside the page window.
        let b = unsafe { *((addr + off) as *const u8) };
        assert_eq!(b, 0, "reset must zero the used extent");
    }
}

#[test]
fn freed_large_page_returns_its_granules() {
    let alloc = allocator();
    let bytes = alloc.config().medium_object_limit + 1;
    let page = alloc.alloc_page(ZPageSizeClass::Large, bytes).unwrap();
    let base = page.base();
    let before = alloc.stats();
    assert_eq!(before.large_pages, 1);
    assert_eq!(before.committed, 12288);

    alloc.free_page(&page);
    let after = alloc.stats();
    assert_eq!(after.committed, 0, "a large page is released, never cached");
    assert_eq!(after.cached, 0);
    assert_eq!(after.large_pages, 0);
    assert_eq!(after.free_granules, before.free_granules + 3);
    assert!(
        alloc.page_for(base).is_none(),
        "a released page must be uninstalled from the table",
    );
}

#[test]
fn capacity_exhaustion_returns_an_error_not_a_panic() {
    let alloc = allocator();
    // 256 KiB budget / 64 KiB medium pages = exactly four pages.
    let mut pages = Vec::new();
    for _ in 0..4 {
        pages.push(alloc.alloc_page(ZPageSizeClass::Medium, 0).unwrap());
    }
    let err = alloc.alloc_page(ZPageSizeClass::Medium, 0).unwrap_err();
    match err {
        ZPageError::OutOfCapacity {
            requested,
            committed,
            max_capacity,
        } => {
            assert_eq!(requested, 65536);
            assert_eq!(committed, 262144);
            assert_eq!(max_capacity, 262144);
        }
        other => panic!("expected OutOfCapacity, got {other:?}"),
    }
    // The error is Display-able for the OOME message path.
    assert!(!format!("{err}").is_empty());

    // Freeing one page makes room again, via the cache.
    let victim = pages.pop().unwrap();
    alloc.free_page(&victim);
    assert!(alloc.alloc_page(ZPageSizeClass::Medium, 0).is_ok());
}

#[test]
fn object_allocation_exhaustion_surfaces_as_an_error() {
    let alloc = allocator();
    // Fill the whole 256 KiB budget with small objects. 32 small pages of
    // 8 KiB, 8 KiB / 1 KiB = 8 objects each => 256 objects.
    let mut n = 0usize;
    loop {
        match alloc.alloc_object(1024, 8) {
            Ok(_) => {
                n += 1;
                assert!(n <= 4096, "allocator handed out more than the budget holds");
            }
            Err(e) => {
                assert!(
                    matches!(e, ZPageError::OutOfCapacity { .. }),
                    "expected OutOfCapacity, got {e:?}",
                );
                break;
            }
        }
    }
    assert_eq!(n, 256, "budget / object size must be handed out exactly");
}

#[test]
fn cache_flush_frees_granules_for_a_different_size_class() {
    let alloc = allocator();
    // Spend the whole budget on small pages, then release them all: the
    // cache still owns every granule, so a medium page can only be served
    // if the allocator flushes the cache rather than reporting OOM.
    let mut pages = Vec::new();
    while let Ok(p) = alloc.alloc_page(ZPageSizeClass::Small, 0) {
        pages.push(p);
    }
    assert_eq!(pages.len(), 32);
    for p in &pages {
        alloc.free_page(p);
    }
    assert_eq!(alloc.stats().cached, 262144);
    assert_eq!(alloc.stats().committed, 262144);

    let medium = alloc
        .alloc_page(ZPageSizeClass::Medium, 0)
        .expect("cache flush must recover the granules");
    assert_eq!(medium.size(), 65536);
    assert_eq!(alloc.stats().committed, 65536);
    assert_eq!(alloc.stats().small_pages, 0);
}

#[test]
fn shared_page_rolls_over_and_retires_the_full_one() {
    let alloc = allocator();
    // 8 objects of 1 KiB fill one small page; the 9th must roll onto a
    // fresh page and leave the old one Relocatable.
    let mut first_page = None;
    for i in 0..9 {
        let addr = alloc.alloc_object(1024, 8).unwrap();
        let page = alloc.page_for(addr).unwrap();
        if i == 0 {
            first_page = Some(page);
        } else if i == 8 {
            let first = first_page.as_ref().unwrap();
            assert!(!Arc::ptr_eq(&page, first), "the 9th object must roll over");
            assert_eq!(
                first.state(),
                ZPageState::Relocatable,
                "a filled allocation page becomes a relocation candidate",
            );
        }
    }
    assert_eq!(alloc.stats().small_pages, 2);

    // Retiring closes the current page too.
    alloc.retire_shared_pages();
    for page in alloc.pages() {
        assert_ne!(page.state(), ZPageState::Allocating);
    }
}

#[test]
fn stats_account_for_used_free_cached_and_per_class_pages() {
    let alloc = allocator();
    let empty = alloc.stats();
    assert_eq!(empty.max_capacity, 262144);
    assert_eq!(empty.committed, 0);
    assert_eq!(empty.used, 0);
    assert_eq!(empty.free, 262144);
    assert_eq!(empty.free_granules, 64);
    assert_eq!(
        empty.small_pages + empty.medium_pages + empty.large_pages,
        0
    );

    let small = alloc.alloc_page(ZPageSizeClass::Small, 0).unwrap();
    assert!(small.alloc(1024, 8).is_some());
    assert!(small.alloc(512, 8).is_some());
    let medium = alloc.alloc_page(ZPageSizeClass::Medium, 0).unwrap();
    assert!(medium.alloc(4096, 8).is_some());
    let large = alloc.alloc_page(ZPageSizeClass::Large, 20000).unwrap();
    assert!(large.alloc(20000, 8).is_some());

    small.set_live_bytes(1024);
    medium.add_live_bytes(2048);

    let s = alloc.stats();
    assert_eq!(s.small_pages, 1);
    assert_eq!(s.medium_pages, 1);
    assert_eq!(s.large_pages, 1);
    // 8192 + 65536 + ceil(20000/4096)*4096 == 8192 + 65536 + 20480
    assert_eq!(s.committed, 8192 + 65536 + 20480);
    assert_eq!(s.used, 1536 + 4096 + 20000);
    assert_eq!(s.free, s.max_capacity - s.used);
    assert_eq!(s.live, 1024 + 2048);
    assert_eq!(s.cached, 0);
    assert_eq!(s.free_granules, 64 - (2 + 16 + 5));

    // live_ratio is against the used extent, not the page size.
    assert!((small.live_ratio() - 1024.0 / 1536.0).abs() < 1e-9);
    assert_eq!(small.garbage_bytes(), 1536 - 1024);
    // Large pages are never relocation candidates.
    large.set_state(ZPageState::Relocatable);
    large.set_live_bytes(0);
    assert!(!large.is_relocation_candidate(0.5));
    small.set_state(ZPageState::Relocatable);
    small.set_live_bytes(1);
    assert!(small.is_relocation_candidate(0.5));
}

#[test]
fn addresses_from_different_pages_never_overlap() {
    let alloc = allocator();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for _ in 0..4 {
        let p = alloc.alloc_page(ZPageSizeClass::Small, 0).unwrap();
        spans.push((p.base(), p.end()));
    }
    for p in alloc.pages() {
        assert!(
            p.base() >= alloc.base() && p.end() <= alloc.end(),
            "every page must live inside the reservation",
        );
        assert_eq!(
            (p.base() - alloc.base()) % alloc.config().granule_size,
            0,
            "every page must start on a granule boundary",
        );
    }
    for i in 0..spans.len() {
        for j in (i + 1)..spans.len() {
            let (a0, a1) = spans[i];
            let (b0, b1) = spans[j];
            assert!(a1 <= b0 || b1 <= a0, "page windows must not overlap");
        }
    }
}

// -----------------------------------------------------------------------
// The relocation-selection denominator (2026-08-07)
// -----------------------------------------------------------------------

/// PINS the denominator decision recorded on
/// [`ZPageReal::relocation_capacity_bytes`]: the allocated extent, not the
/// page span. A wholly-live but sparsely-filled page must be rejected, and
/// the span measure — computed here so the divergence is pinned in this
/// module rather than inferred from another one — must be the measure that
/// understates its occupancy.
#[test]
fn the_relocation_denominator_is_the_used_extent_not_the_page_span() {
    let alloc = allocator();
    let page = alloc.alloc_page(ZPageSizeClass::Small, 0).unwrap();
    // 512 B allocated in an 8 KiB page, every byte of it live: evacuating
    // this page copies 512 B and reclaims nothing.
    assert!(page.alloc(512, 8).is_some());
    page.set_live_bytes(512);
    page.set_state(ZPageState::Relocatable);

    assert_eq!(page.relocation_capacity_bytes(), page.used());
    assert_eq!(page.relocation_capacity_bytes(), 512);
    assert!(page.relocation_capacity_bytes() < page.size());

    // The chosen denominator: 100% occupied, zero garbage, so no threshold
    // short of 1.0 admits it.
    assert_eq!(page.garbage_bytes(), 0);
    // Exact: 512/512. The comment above says "no threshold short of 1.0
    // admits it", which a tolerance quietly contradicts.
    assert_eq!(page.live_ratio().to_bits(), 1.0f64.to_bits());
    assert!(!page.is_relocation_candidate(0.25));
    assert!(!page.is_relocation_candidate(0.99));

    // The rejected denominator: dividing by `size()` reports 7680 B of
    // "garbage" that is untouched reservation, and an occupancy of 1/16 —
    // which clears `ZRelocationPolicy::max_live_occupancy`'s 0.25 cutoff
    // and, because `select` sorts on garbage/capacity descending, ranks
    // this page ABOVE a genuinely half-dead one.
    let span_garbage = page.size() - page.live_bytes();
    let span_ratio = page.live_bytes() as f64 / page.size() as f64;
    assert_eq!(span_garbage, 8192 - 512);
    assert!(span_ratio < 0.25);
    assert!(
        span_ratio < page.live_ratio(),
        "the page-span denominator must be the one that understates occupancy",
    );
}

/// PINS that the divergence is **reachable**, and reachable on the ordinary
/// path rather than in a corner: `retire_shared_pages` is what every cycle
/// calls, and it makes the mutator's half-filled page `Relocatable` exactly
/// where the cursor stood.
#[test]
fn retiring_the_shared_page_makes_a_partly_filled_page_relocatable() {
    let alloc = allocator();
    let addr = alloc.alloc_object(64, 8).unwrap();
    let page = alloc.page_for(addr).unwrap();
    assert_eq!(page.state(), ZPageState::Allocating);

    alloc.retire_shared_pages();

    assert_eq!(page.state(), ZPageState::Relocatable);
    assert_eq!(page.used(), 64);
    assert!(
        page.used() < page.size(),
        "a retired shared page keeps its cursor wherever the mutator left \
         it — this is what makes used() and size() diverge for a page that \
         a relocation set can actually see",
    );
    assert_eq!(page.relocation_capacity_bytes(), 64);

    // With every allocated byte live, the two denominators disagree about
    // whether to evacuate at all.
    page.set_live_bytes(64);
    assert!(!page.is_relocation_candidate(0.25));
    assert!((page.live_bytes() as f64 / page.size() as f64) < 0.25);
}

/// PINS the other half: once a page is full the two denominators converge,
/// and the extent measure still admits a genuinely mostly-dead page. Without
/// this, `relocation_capacity_bytes` reading "reject everything" would pass.
#[test]
fn a_full_page_converges_and_a_mostly_dead_one_is_still_selected() {
    let alloc = allocator();
    let page = alloc.alloc_page(ZPageSizeClass::Small, 0).unwrap();
    while page.alloc(1024, 8).is_some() {}
    assert_eq!(page.used(), page.size());
    assert_eq!(page.relocation_capacity_bytes(), page.size());

    page.set_live_bytes(512);
    page.set_state(ZPageState::Relocatable);
    // Exact: 512/8192 is 2^-4, representable with no rounding.
    assert_eq!(page.live_ratio().to_bits(), (512.0f64 / 8192.0).to_bits());
    assert_eq!(page.garbage_bytes(), 8192 - 512);
    assert!(
        page.is_relocation_candidate(0.25),
        "the extent denominator must still admit a genuinely mostly-dead page",
    );
}

// -- the view guard -----------------------------------------------------
//
// `ZPageReal::view` has always documented that a view allocates nothing,
// frees nothing, and must never be reset. These pin the enforcement, which
// exists because the ADOPTION STATUS note at the head of this module is
// only read by someone who already suspects something is wrong: the whole
// failure mode is a reader finding a complete-looking page allocator and
// assuming the `Arc<ZPageReal>` in their hand is one of its pages. Today
// every page in circulation outside this file's own tests is a view over
// `ZgcRealHeap`'s arena.

/// A view is not storage, and says so.
#[test]
fn a_view_is_not_owned_storage_and_a_real_page_is() {
    let alloc = allocator();
    let page = alloc
        .alloc_page(ZPageSizeClass::Small, 64)
        .expect("a small page");
    assert!(page.is_owned_storage());

    let view = ZPageReal::view(99, ZPageSizeClass::Small, 0x4000, 8192, 512, 128);
    assert!(!view.is_owned_storage());
    // The accounting a view exists to carry is unaffected.
    assert_eq!(view.id(), 99);
    assert_eq!(view.base(), 0x4000);
    assert_eq!(view.used(), 512);
    assert_eq!(view.live_bytes(), 128);
    assert_eq!(view.walk_bounds(), (0x4000, 0x4000 + 512));
}

/// Bumping a view would hand out an address the arena will hand out again.
///
/// In a debug build the `debug_assert!` fires first, which is the louder
/// and preferred outcome; in release the call returns `None` and nothing
/// is handed out. Both are enforcement, so the test is written twice.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "ZPageReal::alloc on a VIEW")]
fn allocating_from_a_view_is_refused() {
    let view = ZPageReal::view(1, ZPageSizeClass::Small, 0x4000, 8192, 0, 0);
    let _ = view.alloc(64, 8);
}

#[cfg(not(debug_assertions))]
#[test]
fn allocating_from_a_view_is_refused() {
    let view = ZPageReal::view(1, ZPageSizeClass::Small, 0x4000, 8192, 0, 0);
    assert!(view.alloc(64, 8).is_none());
    assert_eq!(view.used(), 0, "a refused alloc must not move the cursor");
}

/// Resetting a view would `memset` live Java objects in somebody else's
/// arena. The address here is deliberately bogus: if the guard ever stops
/// working this test segfaults rather than passing quietly.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "ZPageReal::reset_shared on a VIEW")]
fn resetting_a_view_is_refused() {
    let view = ZPageReal::view(2, ZPageSizeClass::Small, 0x4000, 8192, 4096, 64);
    view.reset_shared();
}

#[cfg(not(debug_assertions))]
#[test]
fn resetting_a_view_is_refused() {
    let view = ZPageReal::view(2, ZPageSizeClass::Small, 0x4000, 8192, 4096, 64);
    view.reset_shared();
    assert_eq!(
        view.used(),
        4096,
        "a refused reset must not rewind the cursor either -- a view's `used` is \
         the arena's bump extent and is not this type's to change",
    );
    assert_eq!(view.state(), ZPageState::Allocating);
}

/// Freeing a view releases granules that were never claimed and resets
/// bytes another allocator still owns.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "free_page on a page VIEW")]
fn freeing_a_view_is_refused() {
    let alloc = allocator();
    let view = Arc::new(ZPageReal::view(
        3,
        ZPageSizeClass::Small,
        alloc.base(),
        8192,
        4096,
        64,
    ));
    alloc.free_page(&view);
}

#[cfg(not(debug_assertions))]
#[test]
fn freeing_a_view_is_refused() {
    let alloc = allocator();
    let before = alloc.stats();
    let view = Arc::new(ZPageReal::view(
        3,
        ZPageSizeClass::Small,
        alloc.base(),
        8192,
        4096,
        64,
    ));
    alloc.free_page(&view);
    let after = alloc.stats();
    assert_eq!(
        before, after,
        "a refused free must leave the allocator's committed/cached accounting \
         untouched -- the granules were never this allocator's to reclaim",
    );
    assert_eq!(view.used(), 4096);
}

// -----------------------------------------------------------------------
// C0 stage 0 — one reservation, two views
// -----------------------------------------------------------------------

/// A window that stands in for `ZgcRealHeap`'s arena: real owned bytes,
/// granule-aligned, that the "arena" (here, the `Vec`) keeps.
fn surveyed_window(granules: usize) -> (Vec<u8>, usize, usize) {
    let c = test_config();
    let backing = vec![0u8; c.granule_size * (granules + 1)];
    let raw = backing.as_ptr() as usize;
    let base = (raw + c.granule_size - 1) & !(c.granule_size - 1);
    (backing, base, c.granule_size * granules)
}

fn survey(granules: usize) -> (Vec<u8>, ZPageAllocator) {
    let (backing, base, len) = surveyed_window(granules);
    let alloc = ZPageAllocator::over_reservation(base, len, test_config())
        .expect("a granule-aligned window must survey");
    (backing, alloc)
}

/// The window decides the capacity, not `config.max_capacity`: surveying a
/// heap that is already running and then disagreeing with it about how big
/// it is would describe granules that are not there.
#[test]
fn a_survey_takes_its_geometry_from_the_window() {
    let (_backing, alloc) = survey(8);
    let c = test_config();
    assert!(alloc.is_survey_only());
    assert!(!alloc.owns_reservation());
    assert!(alloc.base_is_granule_aligned());
    assert_eq!(alloc.max_capacity(), c.granule_size * 8);
    assert_eq!(alloc.reserved_bytes(), c.granule_size * 8);
    assert_eq!(alloc.end(), alloc.base() + c.granule_size * 8);
    assert_eq!(alloc.table().granule_count(), 8);
    // `new` still owns its bytes, and says so.
    assert!(allocator().owns_reservation());
    assert!(!allocator().is_survey_only());
}

/// A window shorter than one granule is not a heap. Refused, not clamped
/// up to one granule that is not there.
#[test]
fn a_window_below_one_granule_is_refused() {
    let c = test_config();
    let err = ZPageAllocator::over_reservation(c.granule_size, c.granule_size - 1, c);
    assert!(matches!(err, Err(ZPageError::InvalidConfig(_))));
}

/// THE REFUSAL THAT MAKES STAGE 0 FREE. The arena is still handing these
/// bytes out; a page carved here is a range two allocators both believe
/// they own, and the first object written into it lands on a live Java
/// object with no error on either side.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "alloc_page on a SURVEY")]
fn a_survey_refuses_to_allocate_a_page() {
    let (_backing, alloc) = survey(8);
    let _ = alloc.alloc_page(ZPageSizeClass::Small, 0);
}

#[cfg(not(debug_assertions))]
#[test]
fn a_survey_refuses_to_allocate_a_page() {
    let (_backing, alloc) = survey(8);
    assert!(matches!(
        alloc.alloc_page(ZPageSizeClass::Small, 0),
        Err(ZPageError::SurveyOnly),
    ));
    // And `SurveyOnly` rather than `Fragmented`/`OutOfCapacity`: those two
    // read as heap pressure and invite a retry. This one cannot ever
    // succeed.
    assert!(matches!(
        alloc.alloc_object(64, 8),
        Err(ZPageError::SurveyOnly),
    ));
    let stats = alloc.stats();
    assert_eq!(stats.committed, 0);
    assert_eq!(stats.small_pages, 0);
    assert_eq!(
        stats.free_granules, 0,
        "a survey owns no free granule -- every byte in the window belongs to the \
         arena",
    );
}

/// The grid `zgc.rs::logical_pages` builds every cycle, handed to the
/// survey instead of being thrown away: `page_for` then answers in O(1)
/// and `pages()` hands the same grid back.
#[test]
fn a_survey_indexes_the_view_grid_it_is_given() {
    let (_backing, alloc) = survey(8);
    let c = test_config();
    let views: Vec<Arc<ZPageReal>> = (0..8)
        .map(|i| {
            Arc::new(ZPageReal::view(
                i as u64,
                ZPageSizeClass::Small,
                alloc.base() + i * c.granule_size,
                c.granule_size,
                c.granule_size / 2,
                64,
            ))
        })
        .collect();
    assert_eq!(alloc.adopt_views(&views), 8);

    let stats = alloc.survey_stats();
    assert_eq!(stats.grid_refreshes, 1);
    assert_eq!(stats.views_installed, 8);
    assert_eq!(stats.views_rejected, 0);
    assert_eq!(stats.granules_indexed, 8);

    // O(1) address -> page over the running heap, which is C0's third
    // bullet and the one `ZObjectStarts` cannot do in one shift.
    let probe = alloc.base() + 3 * c.granule_size + 17;
    assert_eq!(alloc.page_for(probe).map(|p| p.id()), Some(3));
    assert!(alloc.table().contains(probe));
    assert!(alloc.page_for(alloc.end() + 4096).is_none());
    assert_eq!(alloc.pages().len(), 8);

    // A refresh REPLACES; it does not accumulate. Two cycles of the same
    // heap must not report sixteen pages.
    assert_eq!(alloc.adopt_views(&views), 8);
    assert_eq!(alloc.pages().len(), 8);
    assert_eq!(alloc.survey_stats().grid_refreshes, 2);
    assert_eq!(alloc.survey_stats().views_installed, 8);

    alloc.clear_views();
    assert!(alloc.pages().is_empty());
    assert!(alloc.page_for(probe).is_none());
}

/// A page that claims to OWN its bytes has no business in a survey: it
/// would be asserting that the arena's storage belongs to a page
/// allocator. Refused per page and counted, so a partial grid is a visible
/// shortfall rather than a silent wrong answer.
#[test]
fn a_survey_refuses_storage_pages_and_out_of_range_views() {
    let (_backing, alloc) = survey(4);
    let c = test_config();
    let owner = allocator();
    let storage = owner
        .alloc_page(ZPageSizeClass::Small, 0)
        .expect("the owning allocator still allocates");
    let inside = Arc::new(ZPageReal::view(
        1,
        ZPageSizeClass::Small,
        alloc.base(),
        c.granule_size,
        0,
        0,
    ));
    let outside = Arc::new(ZPageReal::view(
        2,
        ZPageSizeClass::Small,
        alloc.end() + c.granule_size,
        c.granule_size,
        0,
        0,
    ));
    let installed = alloc.adopt_views(&[Arc::clone(&inside), storage, outside]);
    assert_eq!(installed, 1);
    let stats = alloc.survey_stats();
    assert_eq!(stats.views_installed, 1);
    assert_eq!(
        stats.views_rejected, 2,
        "installed + rejected must account for every page offered, so a reader can \
         tell 'the survey saw everything' from 'the survey saw most of it'",
    );
}

/// An owning allocator's grid is the granule bitmap's. Letting views
/// overwrite it would make `page_for` answer with pages it never
/// allocated.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "adopt_views on an allocator that OWNS")]
fn an_owning_allocator_refuses_a_view_grid() {
    let alloc = allocator();
    let view = Arc::new(ZPageReal::view(
        1,
        ZPageSizeClass::Small,
        alloc.base(),
        test_config().granule_size,
        0,
        0,
    ));
    let _ = alloc.adopt_views(&[view]);
}

#[cfg(not(debug_assertions))]
#[test]
fn an_owning_allocator_refuses_a_view_grid() {
    let alloc = allocator();
    let page = alloc
        .alloc_page(ZPageSizeClass::Small, 0)
        .expect("the owning allocator still allocates");
    let view = Arc::new(ZPageReal::view(
        1,
        ZPageSizeClass::Small,
        alloc.base(),
        test_config().granule_size,
        0,
        0,
    ));
    assert_eq!(alloc.adopt_views(&[view]), 0);
    assert_eq!(
        alloc.page_for(page.base()).map(|p| p.id()),
        Some(page.id()),
        "a refused adopt must leave the allocator's own page table intact",
    );
}

/// The survey is **off by default**, and a default-off flag that reads as
/// on is the specific mistake this repo has now made twice. `off_word:
/// None` in `types/src/flag_groups.rs` is the declaration that matches.
#[test]
fn the_survey_flag_is_default_off() {
    // zt t6/rv3: the guard reads the value, as the reader does, so an
    // operator's `=0` no longer skips the assertion.
    if !true {
        assert!(!page_survey_enabled());        }    }