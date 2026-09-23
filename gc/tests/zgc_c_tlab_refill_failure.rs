// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `ZTlab` when a refill cannot be served.
//!
//! A refill and the object that forced it are requests of wildly different
//! sizes — a whole Small page against at most `max_tlab_alloc`, three orders of
//! magnitude apart at the defaults. A heap with room for the object but not for
//! a fresh chunk is therefore an ordinary state, and reporting the *chunk's*
//! `OutOfCapacity` as the object's answer is the "OOM with most of the heap
//! free" shape this tree has recorded twice
//! (`zgc-oom-with-84-percent-of-the-heap-free`,
//! `zgc-low-end-fragmentation-was-the-starved-tlab-rung`).
//!
//! Also pins the retarget refusal, which used to be a `debug_assert!` — i.e.
//! silent in exactly the build where carving two generations' objects out of
//! one page would actually happen.

use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

use cratonvm_gc::zgc::page::{ZPageAllocator, ZPageConfig, ZPageState};
use cratonvm_gc::zgc::tlab::{ZTlab, ZTlabConfig, ZTlabGeneration, ZTlabHeapHooks};
use cratonvm_gc::zgc::vaddr::ZColor;

/// Three Small pages of budget: one for the shared allocation page the direct
/// path uses, two for the buffer's private pages.
fn three_page_config() -> ZPageConfig {
    ZPageConfig {
        granule_size: 4096,
        small_page_size: 8192,
        medium_page_size: 16384,
        small_object_limit: 2048,
        medium_object_limit: 4096,
        max_capacity: 4096 * 6, // 24 KiB == exactly three Small pages
    }
}

/// A chunk that is a whole Small page, so every refill needs a fresh page and
/// exhaustion arrives in two of them.
fn whole_page_tlab_config() -> ZTlabConfig {
    ZTlabConfig {
        initial_chunk: 8192,
        min_chunk: 8192,
        max_chunk: 8192,
        max_tlab_alloc: 256,
        refill_waste_fraction: 64,
        waste_increment: 0,
        registry_batch: 512,
    }
}

#[derive(Default)]
struct CountingHooks {
    registered: AtomicUsize,
    bytes: AtomicUsize,
    waste: AtomicUsize,
    hash: AtomicI32,
}

impl ZTlabHeapHooks for CountingHooks {
    fn register_allocations(&self, addrs: &[usize], bytes: usize) {
        self.registered.fetch_add(addrs.len(), Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }
    fn allocation_color(&self) -> ZColor {
        ZColor::Remapped
    }
    fn next_hash(&self) -> i32 {
        self.hash.fetch_add(1, Ordering::Relaxed)
    }
    fn note_waste(&self, bytes: usize) {
        self.waste.fetch_add(bytes, Ordering::Relaxed);
    }
}

/// A buffer whose refill is refused must still serve the object from the
/// shared page.
///
/// The setup makes the two budgets visibly different: the shared page is
/// installed first and left almost empty, then the buffer eats the rest of the
/// heap in whole-page chunks. When the next refill has nowhere to come from,
/// there are still kilobytes in the shared page — and the object being asked
/// for is 64 bytes.
#[test]
fn a_refused_refill_falls_back_to_the_shared_page_instead_of_reporting_oom() {
    let pages = ZPageAllocator::new(three_page_config()).expect("geometry must validate");
    let hooks = CountingHooks::default();

    // Install the shared Small page and leave it almost entirely free. This is
    // the memory the buffer must be able to reach once its own refills fail.
    let shared_addr = pages.alloc_object(64, 8).expect("fresh heap");
    let shared = pages.page_for(shared_addr).expect("shared page");
    assert!(shared.remaining() >= 8000);

    let mut tlab = ZTlab::new(whole_page_tlab_config(), ZTlabGeneration::Young);

    // Drive the buffer until the heap is genuinely full. Every refill takes a
    // whole private page, so this ends after exactly two of them.
    let mut served = 0usize;
    let mut last_err = None;
    for _ in 0..4096 {
        match tlab.allocate(64, 8, &pages, &hooks) {
            Ok(_) => served += 1,
            Err(e) => {
                last_err = Some(e);
                break;
            }
        }
    }

    let stats = tlab.stats();
    assert_eq!(
        stats.pages_taken, 2,
        "two private pages is the whole budget"
    );
    assert!(
        stats.direct_allocations > 0,
        "once the refill is refused the object must be served from the shared \
         page, not reported as heap exhaustion (served={served}, stats={stats:?})",
    );
    // Every byte the shared page had left, handed out 64 at a time.
    assert_eq!(
        stats.direct_allocations as usize,
        8128 / 64,
        "the fallback must drain the shared page exactly, not stop early",
    );
    assert!(
        last_err.is_some(),
        "the loop must end in a genuine exhaustion, or it proves nothing",
    );
    assert_eq!(
        served as u64,
        stats.fast_allocations + stats.direct_allocations,
        "every served object came from one of the two paths",
    );
}

/// Retargeting a buffer that still holds a chunk is **refused**, in every build
/// profile, and leaves the buffer exactly as it was.
///
/// The guard used to be a `debug_assert!`. In release the call fell through and
/// wrote the new generation while the buffer kept a page taken for the old one,
/// so the very next refill carved an old-generation chunk out of a young page —
/// the one thing `ZTlab::page_generation` exists to prevent — and *its* guard
/// was a `debug_assert!` too.
#[test]
fn retargeting_a_live_buffer_is_refused_and_changes_nothing() {
    let pages = ZPageAllocator::new(three_page_config()).expect("geometry must validate");
    let hooks = CountingHooks::default();
    let mut tlab = ZTlab::new(whole_page_tlab_config(), ZTlabGeneration::Young);

    tlab.allocate(64, 8, &pages, &hooks).expect("fresh heap");
    let page_id = tlab.page().expect("a chunk implies a page").id();
    assert_eq!(tlab.page_generation(), Some(ZTlabGeneration::Young));

    assert!(
        !tlab.set_generation(ZTlabGeneration::Old),
        "a live chunk must refuse the retarget",
    );
    assert_eq!(tlab.generation(), ZTlabGeneration::Young);
    assert_eq!(tlab.page_generation(), Some(ZTlabGeneration::Young));
    assert_eq!(tlab.page().expect("page kept").id(), page_id);

    // Retire first, and the same call now succeeds — which is the documented
    // way through, and proves the refusal is about the chunk and not the value.
    tlab.retire(&hooks);
    assert!(tlab.set_generation(ZTlabGeneration::Old));
    assert_eq!(tlab.generation(), ZTlabGeneration::Old);
    assert_eq!(tlab.page_generation(), None);
}

/// The module invariant, restated against the fallback path: whatever route an
/// object took, a retired buffer leaves no reserved tail and no `Allocating`
/// page behind.
///
/// The shared page is closed with `retire_shared_pages` first, because that is
/// the *allocator's* page rather than the buffer's — the module invariant says
/// a TLAB releases what it privately holds, not that it can close a page it
/// merely allocated one object from.
#[test]
fn retirement_after_a_refused_refill_still_leaves_no_reserved_tail() {
    let pages = ZPageAllocator::new(three_page_config()).expect("geometry");
    let hooks = CountingHooks::default();
    pages.alloc_object(64, 8).expect("install the shared page");
    let mut tlab = ZTlab::new(whole_page_tlab_config(), ZTlabGeneration::Young);
    for _ in 0..4096 {
        if tlab.allocate(64, 8, &pages, &hooks).is_err() {
            break;
        }
    }
    tlab.retire(&hooks);
    assert!(tlab.reserved_tail().is_none());
    assert!(tlab.is_retired());
    assert!(tlab.page().is_none());

    pages.retire_shared_pages();
    for page in pages.pages() {
        assert_ne!(
            page.state(),
            ZPageState::Allocating,
            "page {} was left Allocating after a full retire",
            page.id(),
        );
    }
}
