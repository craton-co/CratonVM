// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! **A to-space allocation never returns memory inside the relocation set.**
//!
//! This is the property that blocks page-based evacuation, tested as a
//! property rather than as "evacuation appears to work".
//!
//! # The defect these tests are written against
//!
//! `ZgcRealHeap::alloc_in` -- the `ZRelocateContext` hook the page-based
//! relocator asks for to-space -- answered `None` unconditionally, so
//! `CRATONVM_ZGC_PAGE_EVAC` switched nothing and `gc/src/zgc/relocate.rs`
//! stayed a tested, unreachable module
//! (`docs/internal/zgc-round-20260920/gap-b-two-relocators-one-of-them-unreachable.md`).
//! The reason is precise, and it is the first thing
//! [`defect_alloc_serves_the_relocation_sets_own_holes`] pins down:
//!
//! > `Arena::alloc` serves the **free list before it bumps**, and the sweep
//! > that runs immediately before relocation has just rebuilt that free list
//! > out of the complement of the live set -- which includes every garbage
//! > hole inside the pages relocation is about to evacuate. So the most likely
//! > destination `alloc` can return for an evacuated object is a hole inside
//! > that object's own from-space page, and in the limit the destination
//! > aliases the source.
//!
//! `ZRelocateContext::alloc_in`'s contract forbids exactly that: *"It must not
//! be reported by allocating out of the relocation set's own pages -- a
//! destination inside a from-space page would be evacuated again and could
//! alias its source."*
//!
//! # What is asserted, and why in this shape
//!
//! Three things, and the first one is what makes the other two mean
//! something:
//!
//! 1. **The hazard is real on the ordinary path.** A test that only showed
//!    `alloc_to_space` staying out of the set would pass just as happily
//!    against an allocator that could never have gone there in the first
//!    place, on a heap shaped so the free list was empty. So the first test
//!    asserts the *defect*: with the sweep's holes on the list, `Arena::alloc`
//!    hands back a destination inside the relocation set.
//! 2. **The bump-only path cannot, over the whole of to-space, including at
//!    exhaustion.** Not "did not on this input" -- every allocation until the
//!    tail is empty, each one checked against every from-span, plus the floor
//!    comparison that is the actual proof.
//! 3. **The floor cannot be undermined.** The three doors that lower the low
//!    cursor refuse while a window is open, because a floor a later retraction
//!    can walk under is not a floor.
//!
//! # Why the model of "the relocation set" here is the real one
//!
//! `ZgcRealHeap::logical_pages` tiles exactly `[arena_base, arena_base +
//! low_cursor)` into `Z_LOGICAL_PAGE_BYTES` cells and `ZRelocationSet::select`
//! chooses from those, so a relocation set is always a set of **tiles of the
//! low region below the cursor**. These tests build precisely that: a tiled
//! low region, a chosen subset, and that subset's dead spans pushed onto the
//! free list the way the sweep pushes them.

use cratonvm_gc::arena::Arena;

/// Bytes per modelled page. `ZgcRealHeap::Z_LOGICAL_PAGE_BYTES` is larger;
/// the property does not depend on the size, and a small tile keeps the test
/// arena small enough to stay fast.
const PAGE_BYTES: usize = 8 * 1024;

/// Arena big enough for several pages plus a usable to-space tail.
const ARENA_BYTES: usize = 512 * 1024;

/// One modelled from-space page: a half-open arena-offset span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FromPage {
    lo: usize,
    hi: usize,
}

impl FromPage {
    fn contains(&self, offset: usize) -> bool {
        offset >= self.lo && offset < self.hi
    }
}

/// An object placed in the arena: its offset and size.
#[derive(Debug, Clone, Copy)]
struct Placed {
    offset: usize,
    size: usize,
}

/// Fill the low region with objects, tile it into pages, and return
/// `(arena, pages, objects)`.
///
/// Sizes cycle through a small set so the free list ends up holding several
/// distinct size classes -- the small tier is segregated by exact 8-byte
/// class, and a single size would only ever exercise one bucket.
fn populate(arena: &mut Arena, pages_wanted: usize) -> (Vec<FromPage>, Vec<Placed>) {
    let base = arena.base_ptr() as usize;
    let sizes = [48usize, 64, 96, 128, 256, 512];
    let mut objects = Vec::new();
    let mut i = 0usize;
    while arena.used() < pages_wanted * PAGE_BYTES {
        let size = sizes[i % sizes.len()];
        i += 1;
        let p = match arena.alloc(size, 8) {
            Some(p) => p as usize,
            None => break,
        };
        objects.push(Placed {
            offset: p - base,
            size,
        });
    }
    let pages = (0..pages_wanted)
        .map(|k| FromPage {
            lo: k * PAGE_BYTES,
            hi: (k + 1) * PAGE_BYTES,
        })
        .collect();
    (pages, objects)
}

/// Sweep: free every object inside `set`, exactly as the collector's sweep
/// puts the relocation set's garbage holes on the low free list before
/// relocation runs.
///
/// Returns the survivors (the objects still live inside the set), which are
/// what an evacuation would have to copy.
fn sweep_the_set(
    arena: &mut Arena,
    set: &[FromPage],
    objects: &[Placed],
    live_every: usize,
) -> Vec<Placed> {
    let mut survivors = Vec::new();
    for (n, o) in objects.iter().enumerate() {
        if !set.iter().any(|p| p.contains(o.offset)) {
            continue;
        }
        if n % live_every == 0 {
            survivors.push(*o);
            continue;
        }
        arena.add_free_block(o.offset, o.size);
    }
    survivors
}

// ---------------------------------------------------------------------------
// 1. The hazard is real
// ---------------------------------------------------------------------------

/// **`Arena::alloc` hands an evacuation a destination inside its own
/// relocation set.** The defect, asserted, so the property tests below are
/// known to be testing against something.
///
/// This is not a claim about a corner case: the sweep has just put thousands
/// of holes on the free list and the free list is consulted *first*, so the
/// very first request lands in one of them. Nothing in `alloc`'s contract is
/// violated -- this is what a non-moving allocator is supposed to do, which is
/// exactly why a *moving* collector cannot use it for to-space.
#[test]
fn defect_alloc_serves_the_relocation_sets_own_holes() {
    let mut arena = Arena::new(ARENA_BYTES);
    let base = arena.base_ptr() as usize;
    let (pages, objects) = populate(&mut arena, 4);
    // The set is the bottom two pages; the top two stay out of it, so a
    // destination could in principle have been legal.
    let set = &pages[0..2];
    let survivors = sweep_the_set(&mut arena, set, &objects, 8);
    assert!(
        !survivors.is_empty(),
        "the model must keep some survivors, or there is nothing to evacuate"
    );

    let mut inside = 0usize;
    let mut total = 0usize;
    for s in survivors.iter().take(64) {
        let dest = arena
            .alloc(s.size, 8)
            .expect("the free list is full of holes") as usize;
        total += 1;
        if set.iter().any(|p| p.contains(dest - base)) {
            inside += 1;
        }
    }
    assert!(
        inside > 0,
        "Arena::alloc must be shown to return a destination INSIDE the relocation set -- \
         {inside} of {total} landed in it. If this ever reads 0, the rest of this file is \
         testing an allocator that had nowhere else to go and proves nothing."
    );
}

// ---------------------------------------------------------------------------
// 2. The property
// ---------------------------------------------------------------------------

/// **THE PROPERTY.** Every byte `alloc_to_space` returns, for every request,
/// until the tail is exhausted, is outside every page of the relocation set --
/// and the reason is the floor, which is asserted directly.
#[test]
fn to_space_allocation_is_never_inside_the_relocation_set() {
    let mut arena = Arena::new(ARENA_BYTES);
    let base = arena.base_ptr() as usize;
    let (pages, objects) = populate(&mut arena, 6);
    // A DELIBERATELY AWKWARD SET: not a prefix, not contiguous. A floor
    // argument that only worked for "the bottom of the heap" would pass on a
    // prefix and fail here.
    let set: Vec<FromPage> = vec![pages[0], pages[2], pages[3], pages[5]];
    let survivors = sweep_the_set(&mut arena, &set, &objects, 6);
    assert!(!survivors.is_empty());

    let free_before = arena.free_list_bytes();
    assert!(
        free_before > 0,
        "the sweep must have put the set's holes on the free list -- that is the hazard"
    );

    let floor = arena
        .open_to_space()
        .expect("no window is open, so opening one must succeed");
    // Nothing in this test allocates at the large-object end, so
    // `high_cursor == capacity` and `used()` is the low cursor exactly.
    assert_eq!(arena.high_cursor(), arena.capacity());
    assert_eq!(
        floor,
        arena.used(),
        "the floor is the LOW bump cursor at the open"
    );
    for p in set.iter() {
        assert!(
            p.hi <= floor,
            "every relocation-set page must lie strictly below the floor: page \
             [{}, {}) against floor {floor}",
            p.lo,
            p.hi,
        );
    }

    // Evacuate every survivor, then keep going until the tail refuses, so the
    // exhaustion path is covered by the same assertions as the happy one.
    let mut destinations = Vec::new();
    let sizes = [48usize, 64, 96, 128, 256, 512, 1024];
    let mut i = 0usize;
    loop {
        let size = if i < survivors.len() {
            survivors[i].size
        } else {
            sizes[i % sizes.len()]
        };
        i += 1;
        let Some(p) = arena.alloc_to_space(size, 8) else {
            break;
        };
        let off = p as usize - base;
        // (a) the floor, which is the proof
        assert!(
            off >= floor,
            "to-space allocation at offset {off} is below the floor {floor}"
        );
        // (b) the property the floor exists to give, checked independently of
        //     the floor so that a wrong floor cannot make it vacuously true
        for page in set.iter() {
            assert!(
                !page.contains(off),
                "to-space allocation at {off} landed inside relocation-set page \
                 [{}, {})",
                page.lo,
                page.hi,
            );
            assert!(
                off + size <= page.lo || off >= page.hi,
                "to-space allocation [{off}, {}) OVERLAPS relocation-set page [{}, {})",
                off + size,
                page.lo,
                page.hi,
            );
        }
        // (c) the sharpest form: it is not any surviving object's own address
        for s in survivors.iter() {
            assert!(
                off + size <= s.offset || off >= s.offset + s.size,
                "to-space allocation [{off}, {}) aliases a live source object \
                 [{}, {})",
                off + size,
                s.offset,
                s.offset + s.size,
            );
        }
        destinations.push(off);
        assert!(
            arena.is_to_space_offset(off),
            "the arena must agree that {off} is a to-space byte"
        );
    }

    assert!(
        destinations.len() > survivors.len(),
        "the tail must have served every survivor and then some before refusing \
         ({} served, {} survivors)",
        destinations.len(),
        survivors.len(),
    );
    // Strictly ascending: a bump allocator that ever repeats an address has
    // handed the same bytes to two objects.
    for w in destinations.windows(2) {
        assert!(w[0] < w[1], "to-space destinations must ascend: {w:?}");
    }

    let w = arena.close_to_space().expect("the window was open");
    assert_eq!(w.floor(), floor);
    assert_eq!(w.objects(), destinations.len());
    assert!(
        w.refusals() >= 1,
        "the loop ran to exhaustion, so the refusal must be counted"
    );
    // THE FREE LIST WAS NEVER TOUCHED. This is the mechanical statement of
    // the property: the bytes that could have aliased the set are all still
    // free, because the bump path has no way to reach them.
    assert_eq!(
        arena.free_list_bytes(),
        free_before,
        "alloc_to_space must not consume a single free-list byte"
    );
}

/// The window is the switch, and it is off by default: with no window open
/// there is no to-space and `alloc_to_space` refuses.
///
/// This is also the inertness statement for the other two collectors. G1 and
/// the generational heap never call `open_to_space`, so for them the whole
/// feature is this `None`.
#[test]
fn to_space_allocation_is_refused_when_no_window_is_open() {
    let mut arena = Arena::new(64 * 1024);
    assert!(!arena.to_space_is_open());
    assert!(arena.to_space_window().is_none());
    assert!(
        arena.alloc_to_space(64, 8).is_none(),
        "no window, no to-space"
    );
    assert!(!arena.is_to_space_offset(0));

    // And it is exactly as inert after a window has been closed again.
    let _ = arena.open_to_space().expect("open");
    let _ = arena
        .alloc_to_space(64, 8)
        .expect("a fresh arena has a tail");
    let _ = arena.close_to_space().expect("close");
    assert!(
        arena.alloc_to_space(64, 8).is_none(),
        "a closed window must not keep serving"
    );
}

/// The ordinary allocation path is unchanged while a window is open: a
/// mutator (or any other collector sharing this arena type) still gets the
/// free list first.
///
/// Stated as a test because the tempting implementation of this feature is a
/// mode flag on `Arena::alloc`, and a mode flag would change `alloc`'s
/// behaviour for every caller of a type G1 and the generational heap also use.
#[test]
fn opening_a_window_does_not_change_arena_alloc() {
    let mut arena = Arena::new(ARENA_BYTES);
    let base = arena.base_ptr() as usize;
    let (pages, objects) = populate(&mut arena, 3);
    let set = &pages[0..1];
    let _ = sweep_the_set(&mut arena, set, &objects, 6);

    let free_before = arena.free_list_bytes();
    let _floor = arena.open_to_space().expect("open");
    let p = arena.alloc(64, 8).expect("the free list has 64-byte holes") as usize;
    assert!(
        p - base < PAGE_BYTES,
        "Arena::alloc must still serve from the free list -- including from inside the \
         relocation set, which is why relocation may not use it"
    );
    assert!(arena.free_list_bytes() < free_before);
    let _ = arena.close_to_space();
}

// ---------------------------------------------------------------------------
// 3. The floor cannot be undermined
// ---------------------------------------------------------------------------

/// The three doors that lower the low cursor refuse while a window is open.
///
/// A floor is only a floor if nothing can move the ground under it: a
/// retraction below the floor would put the *next* to-space allocation over
/// bytes already handed out, and a deep enough one would put it back inside
/// the relocation set.
#[test]
fn a_retraction_is_refused_while_a_to_space_window_is_open() {
    let mut arena = Arena::new(ARENA_BYTES);
    let (_pages, objects) = populate(&mut arena, 2);
    // Free the tail objects so `retract_cursor_into_free_tail` has a span
    // ending exactly at the cursor to take.
    let cursor_before = arena.used();
    for o in objects.iter().rev().take(4) {
        arena.add_free_block(o.offset, o.size);
    }
    arena.coalesce_free_list();

    let floor = arena.open_to_space().expect("open");
    assert_eq!(
        arena.retract_cursor_to(floor / 2),
        0,
        "retract_cursor_to must refuse under an open window"
    );
    assert_eq!(
        arena.retract_cursor_into_free_tail(),
        0,
        "retract_cursor_into_free_tail must refuse under an open window"
    );
    assert_eq!(
        arena.used(),
        cursor_before,
        "a refused retraction must not have moved the cursor"
    );

    // ...and both work again once the window is closed, so the refusal is a
    // window property and not a permanent disablement.
    let _ = arena.close_to_space().expect("close");
    assert!(
        arena.retract_cursor_into_free_tail() > 0,
        "the tail span must be reclaimable again after the window closes"
    );
}

/// Re-opening an open window is refused. A second open would raise the floor
/// over destinations the first one had already handed out -- the floor would
/// then no longer bound the set it was taken against.
#[test]
fn a_second_open_is_refused() {
    let mut arena = Arena::new(64 * 1024);
    let floor = arena.open_to_space().expect("first open");
    let _ = arena
        .alloc_to_space(256, 8)
        .expect("a fresh arena has a tail");
    assert!(
        arena.open_to_space().is_none(),
        "a nested open must be refused, not silently re-floored"
    );
    assert_eq!(
        arena.to_space_window().map(|w| w.floor()),
        Some(floor),
        "the original floor must survive"
    );
}
