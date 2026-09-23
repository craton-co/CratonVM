// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane-N (wave 3) regression tests: the places where **wave 1's changes and
//! wave 2's changes meet**.
//!
//! # Why this file exists
//!
//! Both earlier waves ran five agents over sibling files with exclusive
//! ownership, no compiler and no sight of each other's work. Each lane's change
//! is internally correct; what neither lane could check is the *composition*.
//! Wave 2 found two such defects in wave 1. This file pins the ones wave 3
//! found, plus the arithmetic that neither wave could execute.
//!
//! Every assertion is on a count, a classification or set membership — no
//! wall-clock bounds, per the rule the integration suite states.

#![cfg(feature = "zgc")]

use cratonvm_gc::zgc::generation::{ZNurseryGeometry, ZNurserySurvivorCensus, ZYoungSpaceVerdict};
use cratonvm_gc::zgc::remembered::{ZPageIdNamespace, ZRememberedSetTable};
use cratonvm_gc::zgc::ZgcRealHeap;

const PAGE_ID: u64 = 7;
const PAGE_BASE: u64 = 0x4000_0000;
const PAGE_SIZE: usize = 2 * 1024 * 1024;

// ===========================================================================
// 1. The base-carrying registration and the page-id namespace latch
// ===========================================================================

/// PREVENTS: the base — the whole point of which is to stop
/// `iterate_slot_addresses` needing a consumer-side `page_id -> base` map whose
/// missing entry is a silently dropped root — being **unreachable on the only
/// heap that ships**.
///
/// One wave added `register_old_page_with_base`, hard-coding
/// `ZPageIdNamespace::Allocator` because that is what its own four call sites
/// meant. Another wave added the namespace latch and made all three of
/// `ZgcRealHeap`'s registrations declare `ContextLocal`, because that heap has
/// no `ZPageAllocator` and its ids are `(addr - arena_base) /
/// Z_LOGICAL_PAGE_BYTES`. Neither wave could see the other. The composition is
/// that the one entry point carrying a base cannot be called by the one driver
/// that would benefit from it — and it fails as a `debug_assert!`, i.e. a panic
/// in every test build, not as a warning.
///
/// `..._in` is the form that takes the namespace, and this is the shape a
/// context-local driver needs.
#[test]
fn a_context_local_driver_can_carry_a_base_through_registration() {
    let table = ZRememberedSetTable::new();
    // What `ZgcRealHeap::card_object` does on the first old-generation store:
    // latches the table to the context-local numbering.
    table.register_old_page_in(ZPageIdNamespace::ContextLocal, PAGE_ID, PAGE_SIZE);
    assert_eq!(
        table.page_id_namespace(),
        Some(ZPageIdNamespace::ContextLocal)
    );

    // The promotion obligation `ZMinorCycleReport::promoted_page_ids`
    // documents: register the page WITH its base, in the caller's own
    // numbering.
    let set = table.register_old_page_with_base_in(
        ZPageIdNamespace::ContextLocal,
        PAGE_ID,
        PAGE_BASE,
        PAGE_SIZE,
    );
    assert_eq!(
        table.page_id_namespace(),
        Some(ZPageIdNamespace::ContextLocal),
        "supplying a base must not change what numbering the table is in",
    );
    assert_eq!(set.base_address(), Some(PAGE_BASE));

    // And the base is what turns a bitmap offset into a root.
    set.remember(1024);
    table.swap_all();
    let mut roots: Vec<u64> = Vec::new();
    let (visited, retained, baseless) = table.iterate_slot_addresses(true, &mut |addr| {
        roots.push(addr);
        true
    });
    assert_eq!(roots, vec![PAGE_BASE + 1024]);
    assert_eq!(visited, 1);
    assert_eq!(
        retained, 1,
        "the retaining scan must carry the edge forward"
    );
    assert_eq!(baseless, 0, "the page has a base, so nothing is skipped");
}

/// The same for the recycle path, which had the same hard-coded namespace.
#[test]
fn recycling_a_context_local_page_id_keeps_the_numbering_and_drops_stale_bits() {
    let table = ZRememberedSetTable::new();
    let set = table.register_old_page_with_base_in(
        ZPageIdNamespace::ContextLocal,
        PAGE_ID,
        PAGE_BASE,
        PAGE_SIZE,
    );
    set.remember(512);
    set.remember(1024);
    assert_eq!(set.bits_set(), 2);

    let fresh = table.recycle_old_page_in(
        ZPageIdNamespace::ContextLocal,
        PAGE_ID,
        PAGE_BASE,
        PAGE_SIZE,
    );
    assert_eq!(
        fresh.bits_set(),
        0,
        "a recycled id must not carry the previous page's edges",
    );
    assert_eq!(
        table.page_id_namespace(),
        Some(ZPageIdNamespace::ContextLocal),
    );
}

/// The defect itself, pinned from the failing side: the base-less shorthand
/// **does** conflict on a context-local table, so a driver that follows the
/// old doc text panics on its first promoted page.
///
/// Written as a `should_panic` rather than a comment because a `debug_assert!`
/// is otherwise only reachable by running into it in production-shaped code,
/// which is exactly how this would have been found.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "page-id namespace conflict")]
fn the_allocator_shorthand_conflicts_on_a_context_local_table() {
    let table = ZRememberedSetTable::new();
    table.register_old_page_in(ZPageIdNamespace::ContextLocal, PAGE_ID, PAGE_SIZE);
    table.register_old_page_with_base(PAGE_ID, PAGE_BASE, PAGE_SIZE);
}

// ===========================================================================
// 2. The nursery survivor census — the round's one live experiment
// ===========================================================================

const MIB: usize = 1024 * 1024;
const ARENA_BASE: usize = 0x1000_0000;
const PAGE_BYTES: usize = 2 * MIB;

fn nursery(floor_pages: usize, span_pages: usize) -> ZNurseryGeometry {
    ZNurseryGeometry {
        base: ARENA_BASE,
        page_bytes: PAGE_BYTES,
        floor: ARENA_BASE + floor_pages * PAGE_BYTES,
        end: ARENA_BASE + (floor_pages + span_pages) * PAGE_BYTES,
    }
}

/// PREVENTS: an operator reading `survivor_cycles=0` and being unable to tell a
/// broken instrument from a real observation.
///
/// The census deliberately refuses to count an empty nursery as a cycle —
/// counting it would drag the percentage toward `Supported`, and an instrument
/// must not drift on its own. The side effect is that `cycles == 0` means three
/// different things at once, and one of them ("nobody wired it up") is a bug in
/// the harness while another ("the nursery is inert") is itself the finding.
///
/// This is the failure mode this tree has recorded before — *the allocation
/// counter was wrong in both directions at once* — so the engagement counter is
/// the thing that makes the reading admissible at all.
#[test]
fn an_unwired_census_is_distinguishable_from_a_wired_but_inert_one() {
    let never_called = ZNurserySurvivorCensus::new();
    let r = never_called.report();
    assert_eq!(r.cycles, 0);
    assert_eq!(r.cycles_observed, 0);
    assert!(!r.is_wired(), "nothing has called it, and it says so");
    assert_eq!(r.verdict(), ZYoungSpaceVerdict::NotEnoughData);

    let wired_but_inert = ZNurserySurvivorCensus::new();
    let empty = ZNurseryGeometry {
        base: ARENA_BASE,
        page_bytes: PAGE_BYTES,
        floor: ARENA_BASE,
        end: ARENA_BASE,
    };
    for _ in 0..10 {
        wired_but_inert.observe_cycle(empty, Vec::new());
    }
    let r = wired_but_inert.report();
    assert_eq!(r.cycles, 0, "an empty nursery is still not evidence");
    assert_eq!(
        r.cycles_observed, 10,
        "but the instrument ran ten times, and THAT is the difference between \
         'fix the wiring' and 'the nursery never fills'",
    );
    assert!(r.is_wired());
    assert_eq!(r.geometry_rejected, 0, "an empty nursery is well-formed");
    assert_eq!(r.verdict(), ZYoungSpaceVerdict::NotEnoughData);
}

/// PREVENTS: an unset young floor being silently reinterpreted as "the nursery
/// is the whole arena", which counts every long-lived old object as a nursery
/// survivor and drives the verdict to `Refuted` for a reason that has nothing
/// to do with the workload.
///
/// `ZNurseryGeometry`'s own doc says it is a struct rather than four arguments
/// because *"getting `base` and `floor` the wrong way round produces a
/// plausible-looking number rather than a failure"* — and then `page_count`
/// clamped `floor` up to `base` and produced exactly that number. A `floor`
/// below `base` is now refused, counted, and logged.
#[test]
fn a_floor_below_the_base_is_refused_rather_than_clamped() {
    let malformed = ZNurseryGeometry {
        base: ARENA_BASE,
        page_bytes: PAGE_BYTES,
        // The shape an unset `gen_young_floor` has.
        floor: 0,
        end: ARENA_BASE + 8 * PAGE_BYTES,
    };
    assert!(!malformed.is_well_formed());
    assert_eq!(
        malformed.page_count(),
        0,
        "a malformed geometry describes no nursery; clamping it to the whole \
         arena is how every old object becomes a 'nursery survivor'",
    );

    let census = ZNurserySurvivorCensus::new();
    for _ in 0..(ZNurserySurvivorCensus::MIN_CYCLES + 2) {
        // Objects spread over the whole arena, i.e. mostly old. Under the old
        // clamp every one of them would have marked a nursery page.
        let everything: Vec<(usize, usize)> = (0..8)
            .map(|i| (ARENA_BASE + i * PAGE_BYTES + 64, 48))
            .collect();
        census.observe_cycle(malformed, everything);
    }
    let r = census.report();
    assert_eq!(r.cycles, 0, "a malformed cycle is not evidence");
    assert_eq!(r.cycles_observed, ZNurserySurvivorCensus::MIN_CYCLES + 2);
    assert_eq!(
        r.geometry_rejected,
        ZNurserySurvivorCensus::MIN_CYCLES + 2,
        "and the operator can see WHY there is no evidence",
    );
    assert_eq!(r.pages_with_survivor, 0);
    assert_eq!(r.survivor_page_percent(), 0);
    assert_eq!(r.verdict(), ZYoungSpaceVerdict::NotEnoughData);
}

/// A well-formed nursery still measures what it says it measures: the
/// bit-per-2-MiB mapping, re-derived rather than trusted.
///
/// `floor` sits four pages above the base, so the bitmap must be indexed from
/// the floor and not from the base — an off-by-`first_page` here would credit
/// survivors to the wrong page and, at the edges, to no page at all.
#[test]
fn the_bit_per_page_mapping_is_indexed_from_the_floor() {
    let g = nursery(4, 6);
    assert_eq!(g.page_count(), 6);

    let census = ZNurserySurvivorCensus::new();
    for _ in 0..ZNurserySurvivorCensus::MIN_CYCLES {
        // The first byte of nursery page 0, the last byte of nursery page 5,
        // and one address in page 3. Three distinct pages of six.
        let survivors = vec![
            (g.floor, 16),
            (g.end - PAGE_BYTES + 8, 16),
            (g.floor + 3 * PAGE_BYTES + 1024, 16),
        ];
        census.observe_cycle(g, survivors);
    }
    let r = census.report();
    assert_eq!(r.cycles, ZNurserySurvivorCensus::MIN_CYCLES);
    assert_eq!(r.pages, 6 * ZNurserySurvivorCensus::MIN_CYCLES);
    assert_eq!(
        r.pages_with_survivor,
        3 * ZNurserySurvivorCensus::MIN_CYCLES,
        "three distinct pages per cycle, and a page with two survivors is \
         still one page",
    );
    assert_eq!(r.survivor_page_percent(), 50);
    assert_eq!(r.verdict(), ZYoungSpaceVerdict::Supported);

    // The boundary: one byte past the end is not in the nursery, and neither
    // is one byte below the floor.
    let census = ZNurserySurvivorCensus::new();
    for _ in 0..ZNurserySurvivorCensus::MIN_CYCLES {
        census.observe_cycle(g, vec![(g.end, 16), (g.floor - 1, 16)]);
    }
    let r = census.report();
    assert_eq!(r.pages_with_survivor, 0);
    assert_eq!(r.survivor_bytes, 0);
}

// ===========================================================================
// 3. The TLAB reservation ledger — two reservation schemes in one allocator
// ===========================================================================

/// 64 MiB: a 64 KiB chunk ceiling (`capacity / 1024`) and a 4 MiB reservation
/// budget (`capacity / 16`), the same geometry the wave-1 share test uses.
const CAPACITY: usize = 64 * 1024 * 1024;

/// PREVENTS: a thread carrying its own spent chunk in `reserved_bytes` forever
/// and shrinking every later chunk it asks for.
///
/// `reserved_bytes` is charged at the ONE carve and released on three paths:
/// `tlab_retire_locked` (this crate's own cells), `reclaim_tlab_tail` (the VM
/// buffer's ordinary retire) and the VM buffer's **next refill**. That third
/// path exists only because a buffer that consumed its chunk to the last byte
/// has no tail, so `Tlab::retire` calls no sink — which means it is the one
/// release nothing else covers, and the one a test has to look at directly.
///
/// The property is not "the number is small", it is "exactly one chunk per
/// thread is outstanding, however many refills it has done".
#[test]
fn a_thread_refilling_repeatedly_holds_exactly_one_chunk_reservation() {
    let heap = ZgcRealHeap::new_shared(CAPACITY);
    heap.set_vm_tlab_enabled(true);
    assert_eq!(
        heap.tlab_reserved_bytes(),
        0,
        "a fresh heap has nothing inside a chunk",
    );

    let mut last = 0usize;
    for refill in 1..=6 {
        let (_, size) = heap
            .refill_tlab(512 * 1024)
            .expect("a 64 MiB heap must serve a chunk");
        assert!(size > 0);
        assert_eq!(
            heap.tlab_reserved_bytes(),
            size,
            "after refill {refill} exactly ONE chunk is outstanding for this \
             thread; a missing release here charges every previous chunk too \
             and the thread shrinks its own successor with its own predecessor",
        );
        last = size;
    }
    assert!(last > 0);
    assert!(
        heap.tlab_reserved_bytes() <= heap.tlab_reservation_budget_bytes(),
        "one thread's outstanding chunk must be inside the budget the share \
         constant names",
    );
}

/// PREVENTS: the byte-budget arm starving a thread outright.
///
/// The two sizing rules bound different quantities — the default divides the
/// budget by a claimant count, `CRATONVM_ZGC_TLAB_RESERVED_BYTES` subtracts the
/// bytes already claimed — and they coexist in one allocator. The byte arm's
/// `budget - reserved` can and must go negative-ish; what it must never do is
/// return `0` or something below the VM's minimum buffer, because
/// `tlab_refill`'s gate turns that into "no TLAB for you" for the life of the
/// process.
///
/// Threads that refill and exit are the documented ratchet on `reserved_bytes`
/// (a chunk handed to a thread that never retires stays counted), so this drives
/// `reserved` past the budget on purpose and then asks whether a fresh thread
/// can still get a buffer.
#[test]
fn the_byte_budget_arm_decays_to_the_floor_and_never_to_nothing() {
    let heap = ZgcRealHeap::new_shared(CAPACITY);
    heap.set_vm_tlab_enabled(true);
    heap.set_tlab_reserved_bytes_sizing(true);
    assert!(heap.tlab_reserved_bytes_sizing());

    let budget = heap.tlab_reservation_budget_bytes();
    assert!(budget > 0);

    let floor = cratonvm_gc::tlab::min_tlab_size();
    let mut sizes = Vec::new();
    // Each thread exits without retiring, so its chunk stays charged: the
    // outstanding total climbs monotonically past the budget.
    for _ in 0..192 {
        let h = std::sync::Arc::clone(&heap);
        let size = std::thread::spawn(move || {
            h.refill_tlab(512 * 1024).map(|(_, size)| size).unwrap_or(0)
        })
        .join()
        .expect("worker must not panic");
        sizes.push(size);
    }

    for (i, w) in sizes.windows(2).enumerate() {
        assert!(
            w[1] <= w[0],
            "the byte arm subtracts what is already claimed, so a later chunk \
             can never be larger than an earlier one (at {i}: {:?} -> {:?})",
            w[0],
            w[1],
        );
    }
    let last = *sizes.last().expect("192 samples");
    assert!(
        last >= floor,
        "a thread must still get a usable buffer once the budget is spent: \
         the clamp floor is {floor} and the answer was {last}. Below it the \
         refill gate refuses and every allocation falls to the per-object \
         path.",
    );
    assert!(
        sizes[0] > last,
        "and the arm must actually bite — {} then {last}",
        sizes[0],
    );
    assert!(
        heap.tlab_reserved_bytes() > budget,
        "the premise of this test is that outstanding claims exceeded the \
         budget ({} vs {budget})",
        heap.tlab_reserved_bytes(),
    );
}

/// PREVENTS: the two reservation schemes double-charging each other.
///
/// Wave 1 counts *claimants* (`vm_tlab_slots`, rebuilt per collection and
/// default ON); wave 2 counts *bytes* (`reserved_bytes`, never reset and
/// default OFF). They are alternative divisors in one function and they are
/// maintained by different code. The ledger must be independent of which one
/// is being consulted: flipping the sizing rule cannot change what is
/// outstanding, and a collection's `retire_all_tlabs` — which opens a new
/// claimant epoch — must not touch the byte ledger either, because a chunk a
/// live thread is still bumping into is still claimed.
#[test]
fn the_claimant_epoch_and_the_byte_ledger_do_not_disturb_each_other() {
    let heap = ZgcRealHeap::new_shared(CAPACITY);
    heap.set_vm_tlab_enabled(true);

    let (_, size) = heap.refill_tlab(512 * 1024).expect("a chunk");
    assert_eq!(heap.tlab_reserved_bytes(), size);

    // The claimant epoch is rebuilt at every collection. The byte ledger is
    // not: these bytes are still inside a live chunk.
    heap.retire_all_tlabs();
    assert_eq!(
        heap.tlab_reserved_bytes(),
        size,
        "a collection rebuilds the CLAIMANT count; it does not un-claim bytes \
         a live VM buffer is still bumping into. Zeroing here would let the \
         byte arm hand out the whole budget again on every cycle, which is \
         precisely the burst window the byte arm exists to close.",
    );

    // And the switch is a sizing rule, not a ledger.
    heap.set_tlab_reserved_bytes_sizing(true);
    assert_eq!(heap.tlab_reserved_bytes(), size);
    heap.set_tlab_reserved_bytes_sizing(false);
    assert_eq!(heap.tlab_reserved_bytes(), size);
}
