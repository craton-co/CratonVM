// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Cross-module seam tests for the production-ZGC submodules under
//! `gc/src/zgc/`.
//!
//! # Why this file exists, and what it is NOT
//!
//! The ten modules under `zgc::` (`vaddr`, `page`, `barrier`, `forwarding`,
//! `metrics`, `remembered`, `mark`, `generation`, `relocate`, `tlab`) were
//! written **in parallel, by authors who could not see one another's work and
//! could not compile against it**. Each is deliberately decoupled: it declares
//! its own trait rather than importing a sibling's concrete type, and each has
//! a thorough in-module `#[cfg(test)]` suite.
//!
//! That means every module's *interior* is tested and every **seam between
//! two modules is not**. Blind parallel authoring has already produced
//! confirmed divergences of exactly that shape in this subsystem:
//!
//! * `barrier.rs` originally *copied* the colored-pointer constants out of the
//!   `zgc.rs` simulation, which uses different bit positions than `vaddr.rs`
//!   actually chose. (Since repaired: `barrier` now re-exports them. The test
//!   [`barrier_reexports_are_bit_identical_to_vaddrs_definitions`] is what
//!   keeps it repaired.)
//! * `remembered.rs` was written against `HEADER_SIZE = 24`; it is 16.
//! * `metrics::ZgcPhase` and `zgc::ZgcPhase` are two different public enums
//!   with the same name and different variant sets, both reachable as
//!   `cratonvm_gc::zgc::…::ZgcPhase`.
//!
//! So: **this file tests seams, not interiors.** A test here that duplicates an
//! in-module unit test is wasted; a test here that fails is a report about two
//! modules disagreeing.
//!
//! # It is an integration test on purpose
//!
//! An integration test binary links the crate from *outside* and sees only its
//! **public** API. That is itself a check: any seam that cannot be assembled
//! here is a seam a future `ZgcRealHeap` (which also lives outside these
//! modules, in `zgc.rs`) cannot assemble either.
//!
//! # Conventions used below
//!
//! * Every test name says **what breaks if it fails**, not what it does.
//! * Every test carries a comment naming the **cross-module assumption** it
//!   pins and the two modules that must agree.
//! * **No wall-clock assertions.** This repo has documented CI flakes from
//!   fixed timing bounds (`docs/known-issues/.../no-wallclock`). Everything
//!   here asserts counts, addresses, set membership and identity.
//! * Geometry is scaled down 1/512 (4 KiB granule, 256 KiB heap) exactly as
//!   `page.rs`'s own tests do, so no test commits hundreds of MiB.
//! * A seam that genuinely **cannot** be assembled from the public API is
//!   written as an `#[ignore]`d test whose comment names the exact
//!   incompatibility. Inventing glue that papers over the gap would destroy
//!   the only evidence that the gap exists.

#![cfg(feature = "zgc")]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use cratonvm_gc::zgc::barrier::{self, ZBarrierContext};
use cratonvm_gc::zgc::forwarding;
use cratonvm_gc::zgc::generation::{self, ZGenerationMarker, ZRememberedSetView};
use cratonvm_gc::zgc::metrics;
use cratonvm_gc::zgc::page;
use cratonvm_gc::zgc::remembered::{self, ZGenerationContext};
use cratonvm_gc::zgc::vaddr;

// Modules are imported by *path* rather than by glob throughout this file.
// That is not style: `zgc::ZgcPhase` and `zgc::metrics::ZgcPhase` are two
// different public types with the same name, `forwarding::ZRelocationSet` and
// `generation::ZGenerationScope` both expose `contains`, and `vaddr::is_good`
// is a free function that shadows `ZGoodMask::is_good`. Qualified paths make
// every one of those unambiguous at the call site.

// ===========================================================================
// Shared fixtures
// ===========================================================================

/// The 1/512-scale geometry `page.rs`'s own tests use: 4 KiB granule, 8 KiB
/// small page, 64 KiB medium page, 256 KiB budget.
///
/// Copied deliberately rather than exported from `page.rs`, because a test
/// helper in `src` would be a `pub` surface nothing in production needs.
fn small_geometry() -> page::ZPageConfig {
    page::ZPageConfig {
        granule_size: 4096,
        small_page_size: 8192,
        medium_page_size: 65536,
        small_object_limit: 8192 / 8,
        medium_object_limit: 65536 / 8,
        max_capacity: 4096 * 64,
    }
}

fn small_allocator() -> Arc<page::ZPageAllocator> {
    Arc::new(
        page::ZPageAllocator::new(small_geometry()).expect("scaled test geometry must validate"),
    )
}

/// Translate `page.rs`'s [`page::ZPageSizeClass`] into `forwarding.rs`'s three
/// bare `u8` constants.
///
/// **This function is a finding, not a convenience.** `page.rs` models the size
/// class as an enum; `forwarding.rs` models it as `ZFWD_SIZE_CLASS_SMALL` /
/// `_MEDIUM` / `_LARGE` `u8`s, explicitly so that it has no dependency on the
/// page module. Nothing in `src` converts between them, so every future caller
/// will hand-roll this mapping, and a caller that gets `Large` wrong silently
/// admits large pages to the relocation set — the one page class
/// [`forwarding::ZRelocationPolicy::relocate_large_pages`] exists to exclude.
fn size_class_index(class: page::ZPageSizeClass) -> u8 {
    match class {
        page::ZPageSizeClass::Small => forwarding::ZFWD_SIZE_CLASS_SMALL,
        page::ZPageSizeClass::Medium => forwarding::ZFWD_SIZE_CLASS_MEDIUM,
        page::ZPageSizeClass::Large => forwarding::ZFWD_SIZE_CLASS_LARGE,
    }
}

// ===========================================================================
// Seam 1: vaddr <-> barrier
// ===========================================================================

/// A [`ZBarrierContext`] whose masks come from a **real**
/// [`vaddr::ZGoodMask`], not from constants re-declared in the test.
///
/// That is the whole point of the fixture: if `vaddr` and `barrier` disagree
/// about which bit means what, this context is where the disagreement becomes
/// an assertion failure instead of a silent mis-classification in production.
struct VaddrBarrierContext {
    mask: vaddr::ZGoodMask,
    marking: AtomicBool,
    relocating: AtomicBool,
    /// `bare from-offset -> bare to-offset`, standing in for a
    /// [`forwarding::ZForwardingTable`]. Kept as a plain map so seam 1 tests
    /// only the vaddr/barrier pair; the forwarding seam is seam 2.
    forwardings: Mutex<HashMap<u64, u64>>,
    marked: Mutex<Vec<u64>>,
    stats: barrier::ZBarrierStats,
    /// When true, `heal_color` adds [`vaddr::Z_COLORED_TAG`] so the healed word
    /// is a *well-formed* vaddr colored word. When false the trait's default
    /// behaviour (`heal_color == good_mask`) is used. See
    /// [`barrier_default_heal_color_produces_a_word_vaddr_rejects`].
    tag_healed_words: bool,
}

impl VaddrBarrierContext {
    fn new(tag_healed_words: bool) -> Self {
        VaddrBarrierContext {
            mask: vaddr::ZGoodMask::new(),
            marking: AtomicBool::new(false),
            relocating: AtomicBool::new(false),
            forwardings: Mutex::new(HashMap::new()),
            marked: Mutex::new(Vec::new()),
            stats: barrier::ZBarrierStats::new(),
            tag_healed_words,
        }
    }

    fn marked_snapshot(&self) -> Vec<u64> {
        self.marked.lock().expect("mark log poisoned").clone()
    }
}

impl ZBarrierContext for VaddrBarrierContext {
    fn good_mask(&self) -> u64 {
        self.mask.good()
    }

    fn heal_color(&self) -> u64 {
        if self.tag_healed_words {
            // `Z_COLORED_TAG` is bit 63 and `Z_OFFSET_MASK` is bits 0..=41, so
            // this satisfies the trait's "heal_color must not overlap
            // address_mask" contract while still producing a word that
            // `vaddr::is_well_formed` accepts.
            vaddr::Z_COLORED_TAG | self.mask.good()
        } else {
            self.mask.good()
        }
    }

    fn is_marking(&self) -> bool {
        self.marking.load(Ordering::Relaxed)
    }

    fn is_relocating(&self) -> bool {
        self.relocating.load(Ordering::Relaxed)
    }

    fn forward(&self, addr: u64) -> Option<u64> {
        // `None` means "this reference must not be used" and diverges via
        // `on_forward_failure`. A test fixture that always resolves returns
        // `Some(identity)` for a miss — the object did not move.
        let map = self.forwardings.lock().expect("forwarding map poisoned");
        Some(map.get(&addr).copied().unwrap_or(addr))
    }

    fn mark_live(&self, addr: u64) {
        self.marked.lock().expect("mark log poisoned").push(addr);
    }

    fn stats(&self) -> &barrier::ZBarrierStats {
        &self.stats
    }
}

/// PINS: `vaddr::color`'s bit layout against `barrier`'s fast-path classifier,
/// in every phase of a full ZGC cycle.
///
/// If `vaddr` and `barrier` ever disagree about which bit is `Marked0` /
/// `Marked1` / `Remapped`, the barrier will classify a correctly-coloured word
/// as *bad* (a permanent slow path, i.e. a silent throughput collapse) or a
/// stale word as *good* (a missed mark, i.e. a use-after-free). This test is
/// the only place those two modules are made to agree.
#[test]
fn a_vaddr_colored_word_is_good_to_the_barrier_in_every_phase_of_a_cycle() {
    let ctx = VaddrBarrierContext::new(true);
    let offset: u64 = 0x1_8000; // 8-aligned, well inside Z_OFFSET_MASK

    // -- Phase 1: Relocate (the quiescent phase a fresh mask starts in) -----
    assert_eq!(
        ctx.mask.phase(),
        vaddr::ZGlobalPhase::Relocate,
        "vaddr::ZGoodMask::new must start quiescent; barrier tests below assume it"
    );
    assert_eq!(ctx.mask.allocation_color(), vaddr::ZColor::Remapped);

    let remapped = vaddr::color(offset, vaddr::ZColor::Remapped);
    // The two modules' fast-path tests must agree, term for term.
    assert!(
        vaddr::is_good(remapped, ctx.mask.good()),
        "vaddr says a Remapped word is bad in the Relocate phase"
    );
    assert_eq!(
        barrier::classify(remapped, ctx.mask.good()),
        barrier::ZFastPath::Good(offset),
        "barrier::classify disagrees with vaddr::color's bit layout: a Remapped \
         word must be Good in the Relocate phase, carrying the bare offset"
    );

    let slot = AtomicU64::new(remapped);
    assert_eq!(barrier::z_load(&slot, &ctx), offset);
    assert_eq!(
        slot.load(Ordering::Relaxed),
        remapped,
        "a fast-path hit must not write the slot"
    );

    // -- Phase 2: Mark ------------------------------------------------------
    let mark_good = ctx.mask.flip_to_mark();
    ctx.marking.store(true, Ordering::Relaxed);
    assert_eq!(
        mark_good,
        vaddr::Z_MARKED0,
        "the first flip_to_mark of a process must select Marked0"
    );
    assert_eq!(ctx.mask.phase(), vaddr::ZGlobalPhase::Mark);
    assert_eq!(ctx.mask.allocation_color(), vaddr::ZColor::Marked0);

    // Remapped is now BAD — that is what makes the barrier the mark loop's
    // work source. It must take the slow path exactly once and heal.
    assert_eq!(
        barrier::classify(remapped, ctx.mask.good()),
        barrier::ZFastPath::Bad(remapped),
        "Remapped must be bad during mark, or concurrent marking has no work source"
    );
    assert_eq!(barrier::z_load(&slot, &ctx), offset);
    let healed = slot.load(Ordering::Relaxed);
    assert_eq!(
        healed,
        vaddr::color(offset, vaddr::ZColor::Marked0),
        "the barrier's self-heal must produce exactly the word vaddr::color \
         would build for the current good colour"
    );
    assert!(
        vaddr::is_well_formed(healed),
        "the barrier healed a slot to {healed:#x}, which vaddr::is_well_formed \
         rejects — barrier and vaddr disagree on the colored-word encoding"
    );
    assert_eq!(ctx.marked_snapshot(), vec![offset]);

    // Second load of the healed slot is a fast-path hit: no new mark.
    assert_eq!(barrier::z_load(&slot, &ctx), offset);
    assert_eq!(
        ctx.marked_snapshot().len(),
        1,
        "a healed slot must not re-enter the slow path"
    );

    // -- Phase 3: MarkComplete ---------------------------------------------
    // The good colour is deliberately UNCHANGED here: nothing has moved yet.
    ctx.mask.set_mark_complete();
    assert_eq!(ctx.mask.phase(), vaddr::ZGlobalPhase::MarkComplete);
    assert_eq!(
        ctx.mask.good(),
        mark_good,
        "set_mark_complete must not change the good mask — every pointer the \
         mutator holds is still correctly coloured"
    );
    assert_eq!(ctx.mask.allocation_color(), vaddr::ZColor::Marked0);
    assert_eq!(barrier::z_load(&slot, &ctx), offset);
    assert_eq!(
        ctx.marked_snapshot().len(),
        1,
        "MarkComplete must not re-arm the barrier for already-good words"
    );

    // -- Phase 4: Relocate again -------------------------------------------
    let moved_to: u64 = 0x2_4000;
    ctx.forwardings
        .lock()
        .expect("forwarding map poisoned")
        .insert(offset, moved_to);
    ctx.relocating.store(true, Ordering::Relaxed);
    ctx.marking.store(false, Ordering::Relaxed);
    let remap_good = ctx.mask.flip_to_remap();
    assert_eq!(remap_good, vaddr::Z_REMAPPED);

    // The Marked0 word is now stale: forward it, and heal to the new address.
    assert_eq!(
        barrier::z_load(&slot, &ctx),
        moved_to,
        "the barrier must return the forwarded address once relocation is on"
    );
    let healed = slot.load(Ordering::Relaxed);
    assert_eq!(
        healed,
        vaddr::color(moved_to, vaddr::ZColor::Remapped),
        "after relocation the healed word must be vaddr::color(new, Remapped)"
    );
    assert!(vaddr::is_well_formed(healed));
    assert_eq!(
        vaddr::color_of(healed),
        Some(vaddr::ZColor::Remapped),
        "vaddr must be able to decode the colour the barrier just wrote"
    );
    assert_eq!(
        ctx.marked_snapshot().len(),
        1,
        "marking is off in the relocate phase; no new mark may be enqueued"
    );

    // Rotation: the NEXT mark cycle must use the other mark bit, so this
    // cycle's marks become next cycle's stale colour with no heap pass.
    let second_mark_good = ctx.mask.flip_to_mark();
    assert_eq!(
        second_mark_good,
        vaddr::Z_MARKED1,
        "the mark bit must rotate; without rotation last cycle's marks would \
         have to be cleared by a heap traversal"
    );
    assert_eq!(ctx.mask.current_marked_bit(), vaddr::Z_MARKED1);
}

/// PINS: that a `vaddr` colored word can never be mistaken for a machine
/// pointer by anything downstream (`page`, `generation`, `ObjectRef`).
///
/// `page.rs` and `generation.rs` traffic exclusively in bare machine addresses.
/// If a colored word leaked into one of them it would be dereferenced. The tag
/// bit is the structural defence, and this test states the two properties every
/// consumer relies on: the tag is set, and the word fails an ordinary
/// `addr < 2^47` user-space plausibility check.
#[test]
fn a_colored_word_carries_the_tag_and_fails_a_machine_pointer_plausibility_check() {
    const USER_SPACE_LIMIT: u64 = 1u64 << 47;

    for color in vaddr::ZColor::ALL {
        let word = vaddr::color(0x4_0000, color);
        assert!(
            vaddr::is_colored_word(word),
            "vaddr::color({color:?}) produced {word:#x} without Z_COLORED_TAG; \
             every audit point in the tree tests bit 63"
        );
        assert!(
            word >= USER_SPACE_LIMIT,
            "colored word {word:#x} is below 2^47 and is therefore \
             indistinguishable from a legal user-space pointer"
        );
        assert!(vaddr::is_well_formed(word));
        assert_eq!(vaddr::offset_of(word), 0x4_0000);
    }

    // Null is the one colored word that IS a plausible (null) pointer, and it
    // must stay untagged so `is_null` and `offset == 0` agree.
    assert!(!vaddr::is_colored_word(vaddr::Z_NULL));
    assert!(vaddr::is_well_formed(vaddr::Z_NULL));
    assert_eq!(vaddr::Z_COLORED_TAG, 1u64 << 63);
}

/// PINS: null handling at the vaddr/barrier seam.
///
/// `vaddr::is_good` reports `false` for null (good-mask form) while
/// `barrier::classify` reports `Good(0)` for it. That is not a bug — the two
/// use different forms deliberately — but it is exactly the kind of asymmetry
/// a caller mixes up, so it is pinned here rather than left to be rediscovered.
#[test]
fn null_is_good_to_the_barrier_but_not_to_vaddrs_good_mask_form() {
    let mask = vaddr::ZGoodMask::new();

    assert!(
        !vaddr::is_good(vaddr::Z_NULL, mask.good()),
        "vaddr::is_good is the good-MASK form; null ANDs to zero, so it reports \
         false and callers must null-check first"
    );
    assert!(
        !vaddr::is_bad(vaddr::Z_NULL, mask.bad()),
        "vaddr::is_bad is the bad-mask form; null must pass it for free"
    );
    assert_eq!(
        barrier::classify(vaddr::Z_NULL, mask.good()),
        barrier::ZFastPath::Good(0),
        "the barrier classifies null as Good so a null reference costs no branch"
    );

    let ctx = VaddrBarrierContext::new(true);
    let slot = AtomicU64::new(vaddr::Z_NULL);
    assert_eq!(barrier::z_load(&slot, &ctx), 0);
    assert_eq!(
        slot.load(Ordering::Relaxed),
        vaddr::Z_NULL,
        "a null slot must never be healed — there is no correct colour for null"
    );
    assert!(ctx.marked_snapshot().is_empty());
}

/// PINS: that concurrent barriers over one slot agree, and that the slot is
/// left in a state `vaddr` still calls well-formed.
///
/// The seam here is not just "the barrier is thread-safe" (barrier.rs tests
/// that in isolation) — it is that the *value the winner publishes* is a legal
/// `vaddr` word. A racing heal that composed the colour differently would leave
/// the heap holding words no other module can decode.
#[test]
fn racing_barriers_on_one_stale_slot_agree_and_leave_a_well_formed_vaddr_word() {
    const THREADS: usize = 4;

    let ctx = Arc::new(VaddrBarrierContext::new(true));
    ctx.mask.flip_to_mark();
    ctx.marking.store(true, Ordering::Relaxed);

    let offset: u64 = 0x9_9000;
    let slot = Arc::new(AtomicU64::new(vaddr::color(
        offset,
        vaddr::ZColor::Remapped,
    )));
    let gate = Arc::new(Barrier::new(THREADS));

    let mut handles = Vec::with_capacity(THREADS);
    for _ in 0..THREADS {
        let ctx = Arc::clone(&ctx);
        let slot = Arc::clone(&slot);
        let gate = Arc::clone(&gate);
        handles.push(std::thread::spawn(move || {
            gate.wait();
            barrier::z_load(&slot, &*ctx)
        }));
    }
    let results: Vec<u64> = handles
        .into_iter()
        .map(|h| h.join().expect("barrier worker panicked"))
        .collect();

    assert_eq!(
        results,
        vec![offset; THREADS],
        "every racing barrier must return the same address for the same \
         observed value; disagreement means one arm skipped the decode"
    );
    let final_word = slot.load(Ordering::Relaxed);
    assert_eq!(final_word, vaddr::color(offset, vaddr::ZColor::Marked0));
    assert!(
        vaddr::is_well_formed(final_word),
        "the winning heal published {final_word:#x}, which vaddr rejects"
    );

    let stats = ZBarrierContext::stats(&*ctx).snapshot();
    assert_eq!(
        stats.heal_cas_wins, 1,
        "exactly one thread may win the heal CAS"
    );
    // Corrected 2026-08-07. This asserted `wins + losses == THREADS`, i.e. that
    // every *thread* is a slow-path entrant. That is false whenever the barrier
    // works: the winner heals the slot to a good colour, so a thread loading
    // after the heal classifies `Good` in `z_barrier` and never calls
    // `load_barrier_slow` at all. The assertion therefore failed precisely when
    // self-healing did its job — 2 entrants, 4 threads — which is the property
    // the whole design exists for.
    //
    // Two separate claims, both worth keeping:
    //   1. no slow-path entrant vanishes silently. `heal_skipped` is the
    //      roll-up for the exits that legitimately attempt no CAS (a `Z_NULL`
    //      slot, a `forward` refusal, a domain-tripwire refusal); the identity
    //      is stated on `ZBarrierStats` and every new early return must pick a
    //      bucket.
    //   2. entrants never exceed threads.
    assert_eq!(
        stats.heal_cas_wins + stats.heal_cas_losses + stats.heal_skipped,
        stats.slow_path_entries,
        "every slow-path entrant must account for its CAS outcome"
    );
    assert!(
        stats.slow_path_entries <= THREADS as u64,
        "a thread that loads after another thread's heal hits the FAST path \
         and is not a slow-path entrant at all — that is what healing is for"
    );
}

/// GAP (`#[ignore]`): `ZBarrierContext`'s **default** `heal_color` is
/// `good_mask()`, which for a `vaddr`-encoded heap omits
/// [`vaddr::Z_COLORED_TAG`].
///
/// A context that takes the default therefore heals slots to words that
/// `vaddr::is_well_formed` rejects and `vaddr::is_colored_word` reports as
/// plain machine pointers — i.e. the barrier's own output would trip
/// `vaddr::debug_assert_plain_word`'s inverse audit and would be invisible to
/// every "is this a colored word?" check in the tree.
///
/// This is a genuine module-boundary gap, not a test artefact: `barrier.rs`
/// re-exports `Z_COLORED_TAG` from `vaddr` but never applies it, and nothing in
/// `src` documents that a vaddr-backed context MUST override `heal_color`.
///
/// Ignored rather than deleted, and rather than "fixed" in the test, because
/// the fix belongs in `src` — either `ZBarrierContext::heal_color`'s default
/// becomes `Z_COLORED_TAG | good_mask()`, or the requirement to override it is
/// written into the trait docs. Un-ignore once one of those lands.
#[test]
#[ignore = "gap: ZBarrierContext::heal_color's default drops vaddr's Z_COLORED_TAG (bit 63)"]
fn barrier_default_heal_color_produces_a_word_vaddr_rejects() {
    let ctx = VaddrBarrierContext::new(false); // use the trait default
    ctx.mask.flip_to_mark();
    ctx.marking.store(true, Ordering::Relaxed);

    let offset: u64 = 0x2_0000;
    let slot = AtomicU64::new(vaddr::color(offset, vaddr::ZColor::Remapped));
    assert_eq!(barrier::z_load(&slot, &ctx), offset);

    let healed = slot.load(Ordering::Relaxed);
    assert!(
        vaddr::is_well_formed(healed),
        "with the DEFAULT heal_color the barrier published {healed:#x}; \
         vaddr::is_well_formed rejects it because Z_COLORED_TAG (bit 63) is clear"
    );
}

/// GAP (`#[ignore]`): `barrier`'s default `address_mask` is
/// [`vaddr::Z_OFFSET_MASK`] (42 bits), but `page.rs` hands out **real machine
/// addresses** from a `Vec<u8>` reservation, and `vaddr` itself declares
/// [`vaddr::Z_MAX_ADDRESS`] to be 47 bits wide.
///
/// So a `ZBarrierContext` over `page.rs`-allocated slots must override
/// `address_mask()`, or the fast path silently truncates every pointer to its
/// low 42 bits. Nothing in `src` connects the two: `page.rs` has no notion of
/// an offset-from-base encoding on its addresses, and `barrier.rs` has no
/// notion that its address payload might be a whole machine pointer.
///
/// The clean fix is for whoever wires `ZgcRealHeap` to store *heap-base-relative
/// offsets* in reference slots (which is what `vaddr`'s docs say a colored word
/// carries) and to convert at the page boundary. Until that decision is made,
/// this assertion is the record of the mismatch.
#[test]
#[ignore = "gap: barrier's 42-bit default address_mask cannot carry page.rs's machine addresses"]
fn barrier_default_address_mask_covers_the_address_space_vaddr_declares() {
    assert!(
        vaddr::Z_OFFSET_MASK >= vaddr::Z_MAX_ADDRESS,
        "barrier's default address_mask is Z_OFFSET_MASK ({:#x}, {} bits) but \
         vaddr declares machine addresses up to Z_MAX_ADDRESS ({:#x}, 47 bits); \
         a context over machine addresses MUST override address_mask()",
        vaddr::Z_OFFSET_MASK,
        vaddr::Z_OFFSET_BITS,
        vaddr::Z_MAX_ADDRESS,
    );
}

// ===========================================================================
// Seam 2: page <-> forwarding
// ===========================================================================

/// Allocate `count` objects of `bytes` each, retire the shared allocation
/// pages, and hand back the addresses.
fn fill_small_pages(allocator: &page::ZPageAllocator, count: usize, bytes: usize) -> Vec<usize> {
    let mut addrs = Vec::with_capacity(count);
    for _ in 0..count {
        addrs.push(
            allocator
                .alloc_object(bytes, 8)
                .expect("scaled heap must have room for this test's objects"),
        );
    }
    // Close the mutator's shared pages so every page is a relocation
    // candidate rather than an allocation target.
    allocator.retire_shared_pages();
    addrs
}

/// PINS: that a relocation set built from **real** `ZPageReal` statistics can
/// be turned into **real** `ZForwardingTable`s keyed by **real** allocated
/// addresses, and that lookups round-trip.
///
/// `forwarding.rs` deliberately knows nothing about `page.rs` — its input is a
/// `PageCandidate` of four integers. Nothing in `src` fills that struct in from
/// a `ZPageReal`, so this test is the first place the two are connected: page
/// id width, `capacity_bytes` vs `ZPageReal::size()`, live-byte units and the
/// size-class encoding all have to line up here or not at all.
#[test]
fn a_relocation_set_built_from_real_pages_round_trips_through_the_forwarding_registry() {
    let allocator = small_allocator();
    let object_bytes = 64usize;
    let addrs = fill_small_pages(&allocator, 266, object_bytes);

    let pages = allocator.pages();
    assert!(
        pages.len() >= 3,
        "the fixture must produce several pages so selection has something to \
         choose between; got {}",
        pages.len()
    );

    // Give every page a sparse live set so the occupancy filter admits it.
    // 1/8 occupancy is comfortably under ZFWD_DEFAULT_MAX_LIVE_OCCUPANCY.
    let mut candidates: Vec<forwarding::PageCandidate> = Vec::new();
    for p in pages.iter() {
        let live = p.used() / 8;
        p.set_live_bytes(live);
        candidates.push(forwarding::PageCandidate {
            page_id: p.id(),
            live_bytes: p.live_bytes(),
            capacity_bytes: p.size(),
            size_class_index: size_class_index(p.size_class()),
        });
    }

    let policy = forwarding::ZRelocationPolicy::default();
    let set = forwarding::ZRelocationSet::select(&candidates, &policy);
    assert!(
        !set.is_empty(),
        "every candidate is a small page at 1/8 occupancy with real garbage; \
         ZRelocationSet::select rejected all of them, so page.rs's live/size \
         units and forwarding.rs's live_bytes/capacity_bytes units disagree"
    );
    assert_eq!(
        set.len(),
        candidates.len(),
        "the whole candidate list fits the default 64 MiB evacuation budget"
    );

    // Selection must name page ids that `page.rs` can still resolve.
    for id in set.page_ids() {
        assert!(
            pages.iter().any(|p| p.id() == id),
            "the relocation set names page {id}, which the allocator does not own"
        );
    }

    let registry = forwarding::ZForwardingRegistry::new();
    registry.install_for_set(&set, forwarding::ZFWD_ASSUMED_AVG_OBJECT_BYTES);
    assert_eq!(
        registry.table_count(),
        set.len(),
        "install_for_set must install exactly one table per selected page"
    );

    // Insert forwardings for addresses the ALLOCATOR actually handed out, keyed
    // page-relative as `forwarding.rs` requires, and read them back.
    //
    // The destinations are synthetic (a flat, 8-aligned range) rather than real
    // to-space addresses on purpose: whether a real `page.rs` address fits
    // ZFWD_TO_BITS is platform-dependent and is pinned separately by
    // `forwarding_to_field_holds_a_real_zpage_heap_address`.
    //
    // Only a handful of entries per page: `install_for_set` sizes each table
    // from `live_bytes / avg_object_bytes` (here 1024/32 = 32 objects -> 64
    // slots), and this test is about the seam, not about probe behaviour at a
    // 1.0 load factor — which `forwarding.rs`'s own suite already covers.
    const PER_PAGE: usize = 4;
    let mut inserted_per_page: HashMap<u64, usize> = HashMap::new();
    // Value is the stored PAYLOAD, not an address — see the note at the
    // `try_insert` below.
    let mut expected: HashMap<(u64, u64), forwarding::ZForwardingPayload> = HashMap::new();
    let mut synthetic_dest: u64 = 0x10_0000;
    for (i, addr) in addrs.iter().enumerate() {
        let page = allocator
            .page_for(*addr)
            .expect("every address alloc_object returned must resolve to a page");
        if !set.contains(page.id()) {
            continue;
        }
        let slot = inserted_per_page.entry(page.id()).or_insert(0);
        if *slot >= PER_PAGE {
            continue;
        }
        *slot += 1;
        let table = registry
            .get(page.id())
            .expect("a selected page must have an installed table");

        let from = (*addr - page.base()) as u64;
        assert!(
            from <= forwarding::ZFWD_MAX_FROM_OFFSET,
            "object {i} sits at page-relative offset {from:#x}, which exceeds \
             forwarding's ZFWD_MAX_FROM_OFFSET ({:#x}) — the from-field is too \
             narrow for page.rs's page sizes",
            forwarding::ZFWD_MAX_FROM_OFFSET
        );

        synthetic_dest += 64;
        // The table stores a `ZForwardingPayload`, not an address: destinations
        // are heap-base-relative so a Linux `mmap` base (~2^47) still fits the
        // 41-bit `to` field. Keep the payload in `expected` rather than the raw
        // word — comparing a decoded address against a stored payload is the
        // exact confusion the newtype exists to make unrepresentable.
        let payload = forwarding::ZForwardingPayload::from_encoded(synthetic_dest);
        let installed = table
            .try_insert(from, payload)
            .expect("try_insert refused a legal (from, to) pair");
        assert_eq!(installed, payload);
        expected.insert((page.id(), from), payload);
    }

    assert!(
        !expected.is_empty(),
        "the fixture inserted no forwardings; the test would prove nothing"
    );

    for ((page_id, from), to) in expected.iter() {
        let table = registry
            .get(*page_id)
            .expect("table vanished from registry");
        assert_eq!(
            table.find_payload(*from),
            Some(*to),
            "forwarding for page {page_id} offset {from:#x} did not round-trip"
        );
    }

    // An address that was never forwarded must miss, not alias onto a neighbour.
    let some_page = set.page_ids()[0];
    let table = registry.get(some_page).expect("table vanished");
    let unforwarded = forwarding::ZFWD_MAX_FROM_OFFSET & !7;
    if !expected.contains_key(&(some_page, unforwarded)) {
        assert_eq!(
            table.find_payload(unforwarded),
            None,
            "an unforwarded offset must miss; a hit means the probe aliased"
        );
    }
}

/// PINS: `forwarding.rs`'s 22-bit from-offset field against `page.rs`'s largest
/// **relocatable** page size.
///
/// The two numbers were chosen independently — `ZFWD_FROM_BITS = 22` in
/// `forwarding.rs` and `ZPAGE_DEFAULT_MEDIUM = 32 MiB` in `page.rs` — and they
/// fit with **exactly zero headroom**: the largest 8-aligned offset in a 32 MiB
/// page is `32 MiB - 8`, and `ZFWD_MAX_FROM_OFFSET` is `32 MiB - 8`. Growing
/// the medium page by one granule silently makes the top of every medium page
/// unforwardable (`try_insert` returns `Unencodable`, and a relocation that
/// cannot publish its forwarding is a lost object).
#[test]
fn forwarding_from_offset_field_exactly_covers_page_rs_largest_relocatable_page() {
    let align = 1u64 << forwarding::ZFWD_ALIGN_SHIFT;

    // Large pages are excluded from relocation by policy, so Medium is the
    // largest page whose offsets must be representable.
    assert!(
        !forwarding::ZRelocationPolicy::default().relocate_large_pages,
        "this bound only holds while large pages are excluded from relocation; \
         relocate_large_pages is now on, so ZFWD_FROM_BITS must be re-derived \
         against the largest LARGE page instead"
    );

    let largest_relocatable = page::ZPAGE_DEFAULT_MEDIUM as u64;
    let highest_object_offset = largest_relocatable - align;
    assert!(
        highest_object_offset <= forwarding::ZFWD_MAX_FROM_OFFSET,
        "page.rs's medium page is {largest_relocatable} B, whose highest \
         8-aligned object offset is {highest_object_offset:#x}, but \
         forwarding.rs's ZFWD_FROM_BITS={} only reaches {:#x}",
        forwarding::ZFWD_FROM_BITS,
        forwarding::ZFWD_MAX_FROM_OFFSET,
    );
    assert!(page::ZPAGE_DEFAULT_SMALL as u64 - align <= forwarding::ZFWD_MAX_FROM_OFFSET);

    // State the zero-headroom fact explicitly so a page-size change trips here
    // with an explanation instead of tripping in production as `Unencodable`.
    assert_eq!(
        forwarding::ZFWD_MAX_FROM_OFFSET + align,
        largest_relocatable,
        "the from-offset field has EXACTLY zero headroom over the medium page \
         size. If page.rs's medium page grew, forwarding.rs's ZFWD_FROM_BITS \
         must grow with it (and ZFWD_TO_BITS must shrink — the two must still \
         tile 63 bits)."
    );

    // And prove it at the encoder rather than only in arithmetic.
    assert!(
        forwarding::ZgcForwardingEntry::pack(
            highest_object_offset,
            forwarding::ZForwardingPayload::from_encoded(0x1000)
        )
        .is_some(),
        "the last object slot of a medium page must be encodable"
    );
    assert!(
        forwarding::ZgcForwardingEntry::pack(
            largest_relocatable,
            forwarding::ZForwardingPayload::from_encoded(0x1000)
        )
        .is_none(),
        "an offset one page past the end must be refused, not wrapped"
    );
}

/// PINS: `forwarding.rs`'s table-size clamp against the **real** `HEADER_SIZE`.
///
/// `ZFWD_MAX_CAPACITY`'s doc comment derives its "unreachable clamp" argument
/// from `HEADER_SIZE = 24`. `HEADER_SIZE` is **16** (`types/src/heap_types.rs`),
/// which means 1.5x more objects fit a medium page than the comment assumes.
/// The clamp still holds — but only just, and it now holds for a reason
/// different from the one written down. This test derives it from
/// `cratonvm_types` so the next `HEADER_SIZE` change fails here rather than
/// silently truncating a forwarding table (which loses relocations).
#[test]
fn forwarding_capacity_clamp_is_unreachable_at_the_real_header_size() {
    let header = cratonvm_gc::heap::HEADER_SIZE;
    assert_eq!(
        header,
        cratonvm_types::HEADER_SIZE,
        "cratonvm_gc::heap re-exports HEADER_SIZE; the two must be one constant"
    );

    let max_live_objects = page::ZPAGE_DEFAULT_MEDIUM / header;
    let capacity = forwarding::zfwd_capacity_for(max_live_objects);
    assert!(
        capacity <= forwarding::ZFWD_MAX_CAPACITY,
        "a fully-live medium page holds {max_live_objects} objects at \
         HEADER_SIZE={header}, which wants a {capacity}-slot forwarding table — \
         past forwarding.rs's ZFWD_MAX_CAPACITY of {}. The clamp would silently \
         under-size the table and lose relocations.",
        forwarding::ZFWD_MAX_CAPACITY
    );
    assert!(
        capacity >= forwarding::ZFWD_MIN_CAPACITY,
        "even a one-object page must get a probe-able table"
    );
    assert_eq!(
        page::ZPAGE_MIN_ALLOC,
        header,
        "page.rs's minimum allocation is what bounds objects-per-page; it must \
         be HEADER_SIZE, or the derivation above is measuring the wrong thing"
    );
}

/// GAP (`#[ignore]`): `forwarding.rs`'s `to`-address field is 44 bits of
/// address (`ZFWD_TO_BITS = 41`, plus a 3-bit alignment shift = 16 TiB), but
/// `page.rs` hands out **real process addresses** and `vaddr` declares the
/// machine address space to be 47 bits (128 TiB).
///
/// On Linux/glibc a large `Vec<u8>` is `mmap`ped near `0x7f…`, which is ~140
/// TiB and does **not** fit `ZFWD_TO_MASK`. `ZgcForwardingEntry::pack` would
/// return `None`, `try_insert` would return `Unencodable`, and the relocation
/// would be dropped. On Windows the same allocation typically lands under 4
/// TiB and the bug is invisible — which is precisely why this must not be a
/// silently-passing test on the dev host.
///
/// The resolution is the same one seam 1's `address_mask` gap needs: decide
/// whether the ZGC data structures carry **heap-base-relative offsets** (which
/// `vaddr`'s 42-bit `Z_OFFSET_MASK` already assumes) or machine addresses, and
/// make `page.rs`, `forwarding.rs` and `barrier.rs` agree on one answer.
#[test]
#[ignore = "gap: forwarding's 44-bit to-address cannot hold a real machine address on Linux"]
fn forwarding_to_field_holds_a_real_zpage_heap_address() {
    let allocator = small_allocator();
    let addr = allocator
        .alloc_object(64, 8)
        .expect("scaled heap must allocate") as u64;

    assert!(
        addr <= forwarding::ZFWD_MAX_PAYLOAD,
        "page.rs handed out real address {addr:#x}, but forwarding.rs's \
         to-field only reaches {:#x} ({} bits + {}-bit alignment shift). \
         Relocations to this page would be silently refused as Unencodable.",
        forwarding::ZFWD_MAX_PAYLOAD,
        forwarding::ZFWD_TO_BITS,
        forwarding::ZFWD_ALIGN_SHIFT,
    );
}

/// PINS: the part of the to-address bound that DOES hold — `forwarding` covers
/// the whole of `vaddr`'s **offset** space, so the offset-based reading of the
/// encoding (the one `vaddr` documents) composes.
#[test]
fn forwarding_to_field_covers_vaddrs_whole_offset_space() {
    assert!(
        forwarding::ZFWD_MAX_PAYLOAD >= vaddr::Z_MAX_HEAP_SIZE - 8,
        "forwarding's to-field ({:#x}) must reach the top of vaddr's heap \
         ({:#x}), or a relocation into the high end of the heap is unencodable",
        forwarding::ZFWD_MAX_PAYLOAD,
        vaddr::Z_MAX_HEAP_SIZE,
    );
    assert!(
        forwarding::ZgcForwardingEntry::pack(
            0,
            forwarding::ZForwardingPayload::from_encoded(vaddr::Z_MAX_HEAP_SIZE - 8)
        )
        .is_some(),
        "the top 8-aligned offset of vaddr's heap must be a legal destination"
    );
}

// ===========================================================================
// Seam 3: page <-> generation
// ===========================================================================

/// A [`ZGenerationMarker`] that credits liveness for a fixed address list.
///
/// It marks **only through `scope`**, which is the contract `generation.rs`
/// enforces on real markers, and it records every rejected probe so a test can
/// prove the scope actually refused an old address.
struct ScopedAddressMarker {
    /// Addresses the tracer "discovers" — deliberately a superset of the roots,
    /// so a scope that leaks would be caught.
    discovered: Vec<u64>,
    bytes_per_object: usize,
    /// Set if the marker was ever handed a scope that admitted an address
    /// listed in `must_be_rejected`.
    leaked: AtomicBool,
    must_be_rejected: Vec<u64>,
    calls: AtomicUsize,
}

impl ScopedAddressMarker {
    fn new(discovered: Vec<u64>, bytes_per_object: usize) -> Self {
        ScopedAddressMarker {
            discovered,
            bytes_per_object,
            leaked: AtomicBool::new(false),
            must_be_rejected: Vec::new(),
            calls: AtomicUsize::new(0),
        }
    }

    fn rejecting(mut self, must_be_rejected: Vec<u64>) -> Self {
        self.must_be_rejected = must_be_rejected;
        self
    }
}

impl ZGenerationMarker for ScopedAddressMarker {
    fn mark_from_roots(
        &self,
        _id: generation::ZGenerationId,
        roots: &[u64],
        scope: &generation::ZGenerationScope,
    ) -> generation::ZMarkReport {
        self.calls.fetch_add(1, Ordering::Relaxed);

        // Every address the tracer would follow must be offered to the scope
        // first — that is the rule the scope exists to make structural.
        for addr in self.must_be_rejected.iter() {
            if scope.admits(*addr as usize) {
                self.leaked.store(true, Ordering::Relaxed);
            }
        }

        let mut report = generation::ZMarkReport::new();
        let mut seen: HashSet<u64> = HashSet::new();
        for addr in roots.iter().chain(self.discovered.iter()) {
            if !seen.insert(*addr) {
                continue;
            }
            if let Some(entry) = scope.page_of(*addr as usize) {
                report.record(entry.page_id, self.bytes_per_object);
                report.objects_marked += 1;
            }
        }
        report
    }
}

fn generational_heap(
    allocator: Arc<page::ZPageAllocator>,
    promotion_age: u32,
) -> generation::ZGenerationalHeap {
    let config = generation::ZGenerationalConfig {
        promotion: generation::ZPromotionPolicy::with_age(promotion_age),
        ..generation::ZGenerationalConfig::default()
    };
    generation::ZGenerationalHeap::new(allocator, config)
}

/// PINS: that `generation.rs`'s young/old page maps and `page.rs`'s page table
/// name the same pages by the same ids.
///
/// `generation.rs` answers "which generation owns this address?" by asking
/// `ZPageAllocator::page_for` and then looking the returned `id()` up in its own
/// maps. If the id `alloc_object` reports and the id `page_for` reports ever
/// diverged, every generation query would answer `None` and a minor cycle would
/// have an empty scope — a collector that silently collects nothing.
#[test]
fn a_young_allocations_page_id_agrees_between_the_allocator_and_the_generation_maps() {
    let allocator = small_allocator();
    let heap = generational_heap(Arc::clone(&allocator), 8);

    let alloc = heap.allocate_young(64, 8).expect("young allocation");
    let page = allocator
        .page_for(alloc.address)
        .expect("page.rs must resolve an address it just handed out");

    assert_eq!(
        page.id(),
        alloc.page_id,
        "ZGenerationAllocation::page_id and ZPageAllocator::page_for disagree"
    );
    assert_eq!(
        heap.page_generation(alloc.page_id),
        Some(generation::ZGeneration::Young),
    );
    assert_eq!(
        heap.generation_of_address(alloc.address),
        Some(generation::ZGeneration::Young),
        "generation_of_address must route through page.rs's O(1) page table"
    );

    // An address outside the reservation belongs to neither generation.
    assert_eq!(heap.generation_of_address(0x10), None);
    assert!(!allocator.in_reserved_range(0x10));
}

/// PINS: that a minor cycle's scope, built from `page.rs` pages, **never**
/// admits an old address — the single property that makes a minor cycle cheaper
/// than a full collection.
///
/// The scope is built by `generation.rs` from `ZPageReal::walk_bounds()`, which
/// is `page.rs`'s "the only bytes a walker may decode" contract. If those
/// bounds ever included untouched reservation, or if old pages leaked into the
/// young snapshot, the marker would be handed addresses it must not follow.
#[test]
fn a_minor_cycle_scope_admits_no_old_address_and_no_unallocated_byte() {
    let allocator = small_allocator();
    // Promotion age well above the cycle count so nothing is promoted here;
    // promotion is tested separately.
    let heap = generational_heap(Arc::clone(&allocator), 64);

    let old = heap.allocate_old(64, 8).expect("old allocation");
    let young: Vec<u64> = (0..40)
        .map(|_| {
            heap.allocate_young(64, 8)
                .expect("young allocation")
                .address as u64
        })
        .collect();

    assert_eq!(
        heap.page_generation(old.page_id),
        Some(generation::ZGeneration::Old)
    );

    // A byte inside a young page but above its allocation extent.
    let young_page = allocator
        .page_for(young[0] as usize)
        .expect("young page must resolve");
    let past_top = young_page.top_addr() as u64;
    assert!(
        past_top < (young_page.end() as u64),
        "the fixture needs a partially-filled page for the 'above top' probe"
    );

    let marker =
        ScopedAddressMarker::new(young.clone(), 64).rejecting(vec![old.address as u64, past_top]);
    let report = heap.collect_young(&young, &generation::ZEmptyRememberedSet, &marker);

    assert_eq!(marker.calls.load(Ordering::Relaxed), 1);
    assert!(
        !marker.leaked.load(Ordering::Relaxed),
        "the minor cycle's ZGenerationScope admitted an OLD address (or a byte \
         above a young page's walk_bounds top). page.rs's walk_bounds and \
         generation.rs's scope construction disagree."
    );
    assert!(
        report.scope_rejected >= 2,
        "the scope must have counted the two refused probes; got {}",
        report.scope_rejected
    );
    assert!(report.scope_admitted >= young.len() as u64);
    assert_eq!(
        report.pages_promoted, 0,
        "promotion age is 64; nothing may promote"
    );
}

/// PINS: that young and old page sets stay disjoint across allocation and
/// collection, using **real** `page.rs` pages on both sides.
///
/// `page.rs`'s allocator recycles page *ids* through its free cache, so a page
/// freed by a minor cycle can be handed straight back out. If `generation.rs`
/// ever failed to drop a freed id from the young map before the allocator
/// reissued it, the same id would appear in both generations and
/// `generation_of_address` would answer arbitrarily.
#[test]
fn young_and_old_page_sets_stay_disjoint_across_a_minor_cycle() {
    let allocator = small_allocator();
    let heap = generational_heap(Arc::clone(&allocator), 64);

    heap.allocate_old(64, 8).expect("old allocation");
    heap.allocate_old(128, 8).expect("old allocation");

    let live: Vec<u64> = (0..10)
        .map(|_| {
            heap.allocate_young(64, 8)
                .expect("young allocation")
                .address as u64
        })
        .collect();
    // A second batch that nothing will keep alive, so its pages are freed and
    // their ids become reusable.
    for _ in 0..200 {
        heap.allocate_young(64, 8).expect("young allocation");
    }

    for _ in 0..3 {
        let marker = ScopedAddressMarker::new(live.clone(), 64);
        heap.collect_young(&live, &generation::ZEmptyRememberedSet, &marker);

        let young_ids: HashSet<u64> = heap.young().snapshot().iter().map(|p| p.id()).collect();
        let old_ids: HashSet<u64> = heap.old().snapshot().iter().map(|p| p.id()).collect();
        let both: Vec<u64> = young_ids.intersection(&old_ids).copied().collect();
        assert!(
            both.is_empty(),
            "page ids {both:?} are in BOTH generations; page.rs recycled an id \
             that generation.rs had not dropped"
        );

        // Every page either generation claims must still be one page.rs owns.
        let owned: HashSet<u64> = allocator.pages().iter().map(|p| p.id()).collect();
        for id in young_ids.iter().chain(old_ids.iter()) {
            assert!(
                owned.contains(id),
                "generation.rs holds page {id}, which page.rs no longer owns"
            );
        }

        // Reallocate so the next cycle has fresh garbage to reclaim.
        for _ in 0..100 {
            heap.allocate_young(64, 8).expect("young allocation");
        }
    }
}

/// PINS: promotion at exactly the policy's age, and that the promoted page
/// leaves the young set and enters the old one.
///
/// # KNOWN FAILURE — this test is the instrument, and it has already fired
///
/// As of this file's authoring, `ZGenerationalHeap::collect_young` ends with
///
/// ```ignore
/// debug_assert_eq!(old_live_before, self.old.live_bytes(),
///     "minor cycle {} changed the old generation's live-byte total ...");
/// ```
///
/// while `sweep_young` promotes by calling `ZOldGeneration::adopt_page(page,
/// live)` on a page whose `live_bytes` it has just set to a **non-zero** value,
/// and `ZOldGeneration::live_bytes()` is `Σ page.live_bytes()` over the old map.
/// So **any minor cycle that promotes anything increases `old.live_bytes()` and
/// trips its own assertion** in every debug build — which is every `cargo test`
/// run.
///
/// That is a real defect in `generation.rs`, not a mis-wiring here: the
/// invariant the assert means to state is "a minor cycle does not *mark* or
/// *sweep* old", and promotion is a legitimate, deliberate write to old. The
/// assert has to exclude the bytes this cycle promoted (`old_live_before +
/// out.bytes_promoted`), or be taken before promotion.
///
/// The test is left enabled rather than `#[ignore]`d because the assertion it
/// makes — "a page is promoted on the cycle its age reaches the policy age" —
/// is the correct one, and a promoting collector that panics is worth failing
/// loudly over.
#[test]
fn a_young_page_is_promoted_on_the_cycle_its_age_reaches_the_policy_age() {
    const PROMOTION_AGE: u32 = 2;

    let allocator = small_allocator();
    let heap = generational_heap(Arc::clone(&allocator), PROMOTION_AGE);
    assert_eq!(heap.promotion_policy().promotion_age, PROMOTION_AGE);

    // One page's worth of survivors, kept alive by the root set every cycle.
    let live: Vec<u64> = (0..8)
        .map(|_| {
            heap.allocate_young(64, 8)
                .expect("young allocation")
                .address as u64
        })
        .collect();
    let page_id = allocator
        .page_for(live[0] as usize)
        .expect("page must resolve")
        .id();
    assert_eq!(
        heap.page_generation(page_id),
        Some(generation::ZGeneration::Young)
    );

    // Cycle 1: the page's age becomes 1, which is below the policy age.
    let marker = ScopedAddressMarker::new(live.clone(), 64);
    let first = heap.collect_young(&live, &generation::ZEmptyRememberedSet, &marker);
    assert_eq!(
        first.pages_promoted, 0,
        "age 1 < promotion_age {PROMOTION_AGE}: nothing may promote yet"
    );
    assert_eq!(
        heap.page_generation(page_id),
        Some(generation::ZGeneration::Young),
        "the survivor page must still be young after one cycle"
    );
    assert!(
        first.young_live_bytes > 0,
        "the marker credited liveness; sweep_young must carry it into the report"
    );

    // Cycle 2: age reaches the policy age, so the page moves to old.
    let marker = ScopedAddressMarker::new(live.clone(), 64);
    let second = heap.collect_young(&live, &generation::ZEmptyRememberedSet, &marker);
    assert_eq!(
        second.pages_promoted, 1,
        "age {PROMOTION_AGE} >= promotion_age {PROMOTION_AGE}: the page must promote"
    );
    assert!(second.bytes_promoted > 0);
    assert_eq!(
        heap.page_generation(page_id),
        Some(generation::ZGeneration::Old),
        "a promoted page must be owned by old and by old alone"
    );
    assert!(
        !heap.young().contains_page(page_id),
        "a promoted page must be removed from the young map before adoption"
    );

    let stats = heap.stats();
    assert_eq!(stats.promoted_pages, 1);
    assert_eq!(stats.minor_cycles, 2);
    assert!(stats.old_pages >= 1);
}

// ===========================================================================
// Seam 4: remembered <-> generation
// ===========================================================================

/// A [`ZGenerationContext`] (remembered.rs's trait) backed by a **real**
/// [`generation::ZGenerationalHeap`] over **real** `page.rs` pages.
///
/// # This adapter does not exist in `src`, and that is a finding
///
/// `remembered.rs` ships `ZRangeGenerationContext`, which models the heap as
/// two contiguous address ranges with a fixed old-page stride. `page.rs` does
/// not lay the heap out that way: young and old pages are interleaved in one
/// granule pool and either generation may own any page. So the shipped context
/// cannot describe a `ZGenerationalHeap`, and nothing in `src` bridges them.
///
/// # And it has to narrow the page id
///
/// `page.rs`, `forwarding.rs` and `generation.rs` all key pages by `u64`.
/// `remembered.rs` keys them by **`u32`**. The narrowing below is checked, but
/// `ZPageAllocator::next_page_id` is a monotonically increasing `u64` that is
/// never recycled downward, so a long-running VM WILL exceed `u32::MAX` page
/// ids and every remembered set past that point would silently alias onto a
/// wrong page. This is the highest-severity type mismatch found at these seams.
struct HeapGenerationContext<'a> {
    heap: &'a generation::ZGenerationalHeap,
}

impl ZGenerationContext for HeapGenerationContext<'_> {
    fn is_old(&self, addr: u64) -> bool {
        self.heap.generation_of_address(addr as usize) == Some(generation::ZGeneration::Old)
    }

    fn is_young(&self, addr: u64) -> bool {
        self.heap.generation_of_address(addr as usize) == Some(generation::ZGeneration::Young)
    }

    fn page_of(&self, addr: u64) -> Option<(u64, usize)> {
        let page = self.heap.allocator().page_for(addr as usize)?;
        // No narrowing check any more: `remembered.rs` was widened to `u64`
        // page ids on 2026-08-07, which is what this seam was flagging. The
        // assertion that used to stand here (`id <= u32::MAX`) is now both
        // dead and misleading, so it is gone rather than left as folklore.
        Some((page.id(), addr as usize - page.base()))
    }
}

/// Bridges `remembered.rs`'s [`remembered::ZRememberedSetTable`] to
/// `generation.rs`'s [`ZRememberedSetView`].
///
/// # The gap this adapter exposes
///
/// The two traits do not compose without a third capability neither module
/// provides:
///
/// * `ZRememberedSetTable` records **slot locations in old pages** — a bit per
///   8-byte grain, page-relative. It knows *where the reference is*.
/// * `ZRememberedSetView::iterate_old_to_young` must yield **young target
///   addresses** — the addresses the minor cycle will use as extra roots. It
///   needs to know *what the reference points at*.
///
/// Going from one to the other requires **reading the old slot's memory**.
/// `remembered.rs` says that read is legitimately its own job ("the card scan
/// happens inside the remembered-set module, which legitimately owns old
/// memory"), but it exposes no API that performs it, and `generation.rs`
/// deliberately gives the minor cycle nothing that can dereference old.
///
/// So a production adapter has to be written by whoever owns the heap, with a
/// raw read. Here the read is supplied as a closure over a map of the stores
/// the test actually performed — modelling the read exactly, without the test
/// taking a raw pointer into the allocator's `Vec<u8>` reservation (which would
/// be UB under stacked borrows, since `ZPageAllocator` only ever exposes
/// `as_ptr()`-derived addresses as `usize`).
struct TableBackedRememberedSetView {
    table: Arc<remembered::ZRememberedSetTable>,
    /// `u32 page id -> page base address`, because `ZRememberedSet` yields
    /// page-relative offsets and knows no base.
    page_bases: HashMap<u64, u64>,
    /// The read `remembered.rs` cannot do and `generation.rs` must not do.
    slot_values: HashMap<u64, u64>,
}

impl ZRememberedSetView for TableBackedRememberedSetView {
    fn iterate_old_to_young(&self, f: &mut dyn FnMut(u64)) {
        for set in self.table.snapshot() {
            let base = match self.page_bases.get(&set.page_id()) {
                Some(b) => *b,
                // A registered old page whose base the caller never recorded.
                // Skipping it silently is exactly the failure mode this
                // adapter's absence from `src` invites; assert instead.
                None => panic!(
                    "remembered set for page {} has no recorded base address",
                    set.page_id()
                ),
            };
            set.iterate(|offset| {
                let slot = base + offset as u64;
                if let Some(target) = self.slot_values.get(&slot) {
                    f(*target);
                }
            });
        }
    }

    fn entry_count(&self) -> usize {
        self.table.total_bits_set()
    }
}

/// PINS: that an old→young store recorded by `remembered.rs`'s store barrier is
/// discovered as a root by `generation.rs`'s minor cycle, and that the two
/// modules agree on where in the page that store landed.
///
/// The bit index is derived from `cratonvm_types::{HEADER_SIZE, REF_FIELD_SIZE}`
/// rather than hard-coded, because hard-coding it is exactly the mistake
/// `remembered.rs` shipped with once (`HEADER_SIZE = 24`; it is 16).
#[test]
fn an_old_to_young_store_becomes_a_minor_cycle_root_at_the_derived_slot() {
    let allocator = small_allocator();
    // High promotion age: this test must not also trip the promotion path.
    let heap = generational_heap(Arc::clone(&allocator), 64);

    // One old object with room for four reference fields, one young target.
    let object_bytes = cratonvm_types::HEADER_SIZE + 4 * cratonvm_types::REF_FIELD_SIZE;
    let old = heap.allocate_old(object_bytes, 8).expect("old allocation");
    let young = heap.allocate_young(64, 8).expect("young allocation");

    let old_page = allocator.page_for(old.address).expect("old page resolves");
    let ctx = HeapGenerationContext { heap: &heap };
    assert!(
        ctx.is_old(old.address as u64),
        "the old object must read as old"
    );
    assert!(
        ctx.is_young(young.address as u64),
        "the young object must read as young"
    );

    // Register the old page with the remembered set, using page.rs's own size.
    let table = Arc::new(remembered::ZRememberedSetTable::new());
    let old_page_id = old_page.id();
    let set = table.register_old_page(old_page_id, old_page.size());
    assert_eq!(set.page_size(), old_page.size());

    // The store: field 3 of the old object now points at the young object.
    let field_index = 3usize;
    let field_offset = cratonvm_types::HEADER_SIZE + field_index * cratonvm_types::REF_FIELD_SIZE;
    let field_addr = old.address as u64 + field_offset as u64;

    let store_barrier = remembered::ZStoreBarrier::new(Arc::clone(&table));
    let outcome = store_barrier.on_reference_store(&ctx, field_addr, young.address as u64);
    assert_eq!(
        outcome,
        remembered::ZStoreBarrierOutcome::Precise,
        "an old-slot -> young-value store must be recorded precisely; a Coarsened \
         or Filtered outcome means remembered.rs and generation.rs disagree about \
         which addresses are old and which are young"
    );

    // -- The derived-index check -------------------------------------------
    //
    // Both modules must agree that the bit for this store is at
    // (page_relative_field_offset / grain). Deriving it from cratonvm_types is
    // what makes a HEADER_SIZE change fail here instead of silently shifting
    // the bit.
    let page_relative = field_addr as usize - old_page.base();
    let expected_bit = page_relative / remembered::Z_REMSET_GRAIN_BYTES;
    assert_eq!(
        remembered::Z_REMSET_GRAIN_BYTES,
        cratonvm_types::REF_FIELD_SIZE,
        "the remembered-set grain must equal one reference slot, or a set bit \
         no longer names exactly one field"
    );
    assert_eq!(
        page_relative,
        (old.address - old_page.base()) + field_offset,
        "page-relative slot arithmetic disagrees between page.rs's base and the \
         object layout cratonvm_types describes"
    );
    assert!(
        set.is_remembered(page_relative),
        "the store barrier recorded nothing at page-relative offset \
         {page_relative} (bit {expected_bit}); remembered.rs's page_of and \
         page.rs's base() disagree"
    );
    assert_eq!(set.bits_set(), 1, "exactly one grain must be dirtied");
    // A neighbouring field must NOT be dirty — this is the precision claim.
    assert!(
        !set.is_remembered(page_relative - remembered::Z_REMSET_GRAIN_BYTES),
        "the adjacent reference slot was dirtied too; the grain is wider than \
         one reference field"
    );

    // -- Now drive a minor cycle through the adapter ------------------------
    let mut page_bases = HashMap::new();
    page_bases.insert(old_page_id, old_page.base() as u64);
    let mut slot_values = HashMap::new();
    slot_values.insert(field_addr, young.address as u64);

    let view = TableBackedRememberedSetView {
        table: Arc::clone(&table),
        page_bases,
        slot_values,
    };
    assert_eq!(view.entry_count(), 1);

    // Roots are EMPTY: the only way the young object can survive is through the
    // remembered set. That is the whole point of the test.
    let marker =
        ScopedAddressMarker::new(Vec::new(), 64).rejecting(vec![old.address as u64, field_addr]);
    let report = heap.collect_young(&[], &view, &marker);

    assert!(
        !marker.leaked.load(Ordering::Relaxed),
        "the minor cycle's scope admitted an old address supplied through the \
         remembered-set path"
    );
    assert_eq!(
        report.remembered_entries, 1,
        "the minor cycle must count the remembered-set entry it consumed"
    );
    assert_eq!(
        report.roots_scanned, 1,
        "the remembered-set target must have been promoted to a root; with an \
         empty caller root set it is the ONLY root"
    );
    assert!(
        report.young_live_bytes > 0,
        "the young object reachable only from old was not kept alive — the \
         old->young edge was lost between remembered.rs and generation.rs"
    );
    assert_eq!(
        heap.generation_of_address(young.address),
        Some(generation::ZGeneration::Young),
        "the young object's page must survive the cycle"
    );
}

/// PINS: that a store into a *young* slot is filtered, and that a store of a
/// pointer to an *old* object is filtered — i.e. the store barrier's two
/// generation tests are wired to the same generation oracle the minor cycle
/// uses.
///
/// A barrier that mis-answers either direction is not merely slow: recording
/// old→old edges floods the set (a minor cycle that scans everything), and
/// failing to record a real old→young edge loses a live object.
#[test]
fn the_store_barrier_filters_exactly_the_edges_a_minor_cycle_does_not_need() {
    let allocator = small_allocator();
    let heap = generational_heap(Arc::clone(&allocator), 64);

    let old_a = heap.allocate_old(64, 8).expect("old allocation");
    let old_b = heap.allocate_old(64, 8).expect("old allocation");
    let young = heap.allocate_young(64, 8).expect("young allocation");

    let ctx = HeapGenerationContext { heap: &heap };
    let table = Arc::new(remembered::ZRememberedSetTable::new());
    for page in heap.old().snapshot() {
        table.register_old_page(page.id(), page.size());
    }
    let sb = remembered::ZStoreBarrier::new(Arc::clone(&table));

    // young slot <- young value: no edge worth recording.
    assert_eq!(
        sb.on_reference_store(&ctx, young.address as u64, young.address as u64),
        remembered::ZStoreBarrierOutcome::Filtered,
        "a store into a young slot must be filtered by the destination test"
    );
    // old slot <- old value: found by the old trace, not the remembered set.
    assert_eq!(
        sb.on_reference_store(&ctx, old_a.address as u64, old_b.address as u64),
        remembered::ZStoreBarrierOutcome::Filtered,
        "an old->old edge must be filtered by the value test"
    );
    // old slot <- null: no edge at all.
    assert_eq!(
        sb.on_reference_store(&ctx, old_a.address as u64, 0),
        remembered::ZStoreBarrierOutcome::Filtered,
    );
    // old slot <- young value: the one case that must be recorded.
    assert_eq!(
        sb.on_reference_store(&ctx, old_a.address as u64, young.address as u64),
        remembered::ZStoreBarrierOutcome::Precise,
    );

    assert_eq!(
        table.total_bits_set(),
        1,
        "exactly one of the four stores may reach the bitmap"
    );
    let stats = sb.stats();
    assert_eq!(stats.stores_seen, 4);
    assert_eq!(stats.remembered_precise, 1);
    assert_eq!(stats.remembered_coarse, 0);
}

// ===========================================================================
// Seam 5: metrics <-> everything
// ===========================================================================

/// PINS: that `tsv_header()` and `to_tsv_row()` emit the same number of
/// columns.
///
/// A one-column drift shifts every value into the wrong column and corrupts
/// every downstream analysis **without erroring anywhere** — the failure is
/// only visible as an implausible number in a report weeks later. The two
/// functions are separate loops over `ZgcPhase::ALL` plus separate hand-written
/// prefixes, so nothing but this test couples them.
#[test]
fn the_metrics_tsv_header_and_row_have_identical_column_counts() {
    let m = metrics::ZgcMetrics::new();
    m.set_run_label("zgc-seam-test");

    // Drive a full synthetic cycle: every phase, then the cycle record.
    for (i, phase) in metrics::ZgcPhase::ALL.iter().enumerate() {
        m.record_phase(*phase, 1_000 * (i as u64 + 1));
    }
    m.record_allocation_stall(4_242);
    m.record_cycle(4 * 1024 * 1024, 1024 * 1024, 16 * 1024 * 1024);

    let header = metrics::ZgcMetrics::tsv_header();
    let row = m.to_tsv_row();
    let header_cols: Vec<&str> = header.split('\t').collect();
    let row_cols: Vec<&str> = row.split('\t').collect();

    assert_eq!(
        header_cols.len(),
        row_cols.len(),
        "TSV header has {} columns and the row has {}; every downstream \
         aggregation of this file is silently mis-aligned.\nheader: {header}\nrow:    {row}",
        header_cols.len(),
        row_cols.len(),
    );

    // The column count is also a contract with the phase enum: 14 fixed columns
    // plus 4 per phase. Stating it here means adding a phase without extending
    // both loops fails as a count mismatch with a comprehensible message.
    assert_eq!(
        header_cols.len(),
        14 + 4 * metrics::ZGC_PHASE_COUNT,
        "the TSV shape must be 14 fixed columns + 4 per ZgcPhase"
    );
    assert_eq!(
        metrics::ZgcPhase::ALL.len(),
        metrics::ZGC_PHASE_COUNT,
        "ZgcPhase::ALL and ZGC_PHASE_COUNT disagree; every per-phase array is \
         sized from the constant and indexed from the array"
    );

    // No column may be empty, or a parser cannot tell a missing value from a
    // present one.
    for (i, col) in row_cols.iter().enumerate() {
        assert!(
            !col.is_empty(),
            "TSV column {i} ({}) is empty in the row",
            header_cols[i]
        );
    }
    assert_eq!(row_cols[0], "zgc-seam-test");
    assert_eq!(row_cols[1], "1", "one cycle was recorded");
}

/// PINS: that `phases_run_concurrently == false` charges **everything** as
/// stop-the-world, so no report can claim a concurrent share the collector did
/// not deliver.
///
/// This is the flag that separates "ZGC's phases are concurrent by design" from
/// "this implementation ran them inside one safepoint". Every other module
/// records phases through this one switch; if the switch leaked even one phase
/// into the concurrent column, the headline STW share — the number the whole
/// `zgc-real-fullsuite-regression` investigation turns on — would be wrong.
#[test]
fn a_non_concurrent_run_reports_exactly_zero_concurrent_nanoseconds() {
    let m = metrics::ZgcMetrics::new();
    m.set_phases_run_concurrently(false);

    for phase in metrics::ZgcPhase::ALL {
        m.record_phase(phase, 1_000);
        assert!(
            m.phase_was_stw(phase),
            "with phases_run_concurrently == false, {phase:?} must be charged STW"
        );
    }

    assert_eq!(
        m.total_concurrent_ns(),
        0,
        "with phases_run_concurrently == false the concurrent total must be \
         EXACTLY zero, not merely small"
    );
    assert_eq!(
        m.total_stw_ns(),
        1_000 * metrics::ZGC_PHASE_COUNT as u64,
        "every phase's time must land in the STW total"
    );
    assert_eq!(m.stw_share(), 1.0);

    // Flipping the switch must move exactly the concurrent-by-design phases.
    m.set_phases_run_concurrently(true);
    let by_design_stw = metrics::ZgcPhase::ALL.iter().filter(|p| p.is_stw()).count();
    assert_eq!(
        m.total_stw_ns(),
        1_000 * by_design_stw as u64,
        "after the flip, only phases whose is_stw() is true may be STW"
    );
    assert_eq!(
        m.total_concurrent_ns(),
        1_000 * (metrics::ZGC_PHASE_COUNT - by_design_stw) as u64,
    );
    assert_eq!(
        m.total_stw_ns() + m.total_concurrent_ns(),
        1_000 * metrics::ZGC_PHASE_COUNT as u64,
        "the two totals must partition the recorded time; a phase counted twice \
         or not at all makes stw_share meaningless"
    );
}

/// PINS: `metrics::ZgcPhase`'s TSV keys as an external contract — they are the
/// column names, so a rename breaks every already-collected aggregation.
#[test]
fn every_metrics_phase_key_is_unique_and_round_trips() {
    let mut keys: HashSet<&'static str> = HashSet::new();
    let mut indices: HashSet<usize> = HashSet::new();
    for phase in metrics::ZgcPhase::ALL {
        assert!(
            keys.insert(phase.key()),
            "duplicate TSV key {:?} — two phases would share a column",
            phase.key()
        );
        assert!(
            indices.insert(phase.index()),
            "duplicate index for {phase:?} — two phases share a counter slot"
        );
        assert_eq!(
            metrics::ZgcPhase::from_key(phase.key()),
            Some(phase),
            "{phase:?}'s key does not round-trip"
        );
        assert!(phase.index() < metrics::ZGC_PHASE_COUNT);
        assert!(!phase.label().is_empty());
        assert!(
            !phase.key().contains('\t'),
            "a TSV key containing a tab would split the header row"
        );
    }
    assert_eq!(keys.len(), metrics::ZGC_PHASE_COUNT);
}

/// PINS: that the two public `ZgcPhase` enums reachable under `zgc::` are NOT
/// interchangeable, and names the consequence.
///
/// `cratonvm_gc::zgc::ZgcPhase` (the simulation's) and
/// `cratonvm_gc::zgc::metrics::ZgcPhase` are different types with the same name
/// and different variant sets:
///
/// * the simulation has `None` and `ConcurrentRemap`, which metrics has no
///   counter for at all;
/// * metrics has `ConcurrentMarkContinue` and `Sweep`, which the simulation
///   cannot express.
///
/// A collector driven by the simulation's phase enum therefore **cannot record
/// its own remap phase**: the time simply vanishes from the report, which reads
/// as "remapping is free". This is not a naming nit.
#[test]
fn the_simulations_phase_enum_cannot_be_recorded_by_the_metrics_module() {
    // Present in the simulation, absent from metrics.
    // 2026-08-07: metrics gained the counter, exactly as the assertion that
    // used to stand here demanded. This is now a MAPPING assertion rather than
    // an absence one — the simulation runs a remap phase, so metrics must have
    // somewhere to put its time or that time reads as zero.
    assert_eq!(
        metrics::ZgcPhase::from_key("concurrent_remap"),
        Some(metrics::ZgcPhase::ConcurrentRemap),
        "the simulation runs a remap phase; metrics must have a counter for it \
         or the time silently reads as zero"
    );
    assert!(!metrics::ZgcPhase::ConcurrentRemap.is_stw());
    // Present in metrics, absent from the simulation.
    assert!(metrics::ZgcPhase::from_key("sweep").is_some());
    assert!(metrics::ZgcPhase::from_key("concurrent_mark_continue").is_some());

    // The overlap that DOES exist must keep matching names, because the two
    // enums are read side by side in logs.
    for key in [
        "pause_mark_start",
        "concurrent_mark",
        "pause_mark_end",
        "concurrent_process_non_strong_refs",
        "concurrent_reset_relocation_set",
        "pause_relocate_start",
        "concurrent_relocate",
    ] {
        assert!(
            metrics::ZgcPhase::from_key(key).is_some(),
            "metrics lost the `{key}` phase, which the simulation still has"
        );
    }
}

// ===========================================================================
// Seam 6: constants-consistency across every ZGC module
// ===========================================================================

/// PINS: that every ZGC module's view of the shared layout facts agrees with
/// `cratonvm_types` and with each other.
///
/// Each module declares its own constants so it can be reviewed independently.
/// That is a reasonable decision **only** if something checks them against one
/// another; without this test, "`remembered.rs` believes `HEADER_SIZE` is 24"
/// is a class of bug that ships. Every message below names both disagreeing
/// modules.
#[test]
fn every_zgc_module_agrees_with_cratonvm_types_on_the_shared_layout_constants() {
    // -- Object header ------------------------------------------------------
    assert_eq!(
        cratonvm_gc::heap::HEADER_SIZE,
        cratonvm_types::HEADER_SIZE,
        "gc::heap and cratonvm_types disagree on HEADER_SIZE"
    );
    assert_eq!(
        page::ZPAGE_MIN_ALLOC,
        cratonvm_types::HEADER_SIZE,
        "zgc::page (ZPAGE_MIN_ALLOC) and cratonvm_types (HEADER_SIZE) disagree \
         on the smallest possible object"
    );
    assert_eq!(
        cratonvm_types::HEADER_SIZE % remembered::Z_REMSET_GRAIN_BYTES,
        0,
        "zgc::remembered's grain ({}) does not divide cratonvm_types::HEADER_SIZE \
         ({}); a reference field would straddle two remembered-set bits",
        remembered::Z_REMSET_GRAIN_BYTES,
        cratonvm_types::HEADER_SIZE,
    );

    // -- Reference / slot width --------------------------------------------
    assert_eq!(
        remembered::Z_REMSET_GRAIN_BYTES,
        cratonvm_types::REF_FIELD_SIZE,
        "zgc::remembered and cratonvm_types disagree on the reference slot width"
    );
    assert_eq!(
        remembered::Z_REMSET_GRAIN_BYTES,
        cratonvm_types::REF_ELEMENT_SIZE,
        "zgc::remembered and cratonvm_types disagree on the reference ARRAY \
         element width"
    );
    assert_eq!(
        cratonvm_types::SLOT_SIZE % remembered::Z_REMSET_GRAIN_BYTES,
        0,
        "zgc::remembered's grain does not divide cratonvm_types::SLOT_SIZE"
    );
    assert_eq!(remembered::Z_REMSET_BITS_PER_WORD, 64);
    assert_eq!(
        remembered::Z_REMSET_BYTES_PER_WORD,
        remembered::Z_REMSET_GRAIN_BYTES * remembered::Z_REMSET_BITS_PER_WORD,
    );

    // -- Object alignment: three modules, one grid -------------------------
    let vaddr_align = vaddr::Z_OBJECT_ALIGNMENT;
    let page_align = page::ZPAGE_OBJECT_GRID as u64;
    let forwarding_align = 1u64 << forwarding::ZFWD_ALIGN_SHIFT;
    let remembered_align = remembered::Z_REMSET_GRAIN_BYTES as u64;
    assert_eq!(
        vaddr_align, page_align,
        "zgc::vaddr (Z_OBJECT_ALIGNMENT) and zgc::page (ZPAGE_OBJECT_GRID) \
         disagree on the object grid"
    );
    assert_eq!(
        vaddr_align,
        forwarding_align,
        "zgc::vaddr (Z_OBJECT_ALIGNMENT) and zgc::forwarding (ZFWD_ALIGN_SHIFT) \
         disagree on the object grid; forwarding drops the low \
         {} bits of every address it stores",
        forwarding::ZFWD_ALIGN_SHIFT
    );
    assert_eq!(
        vaddr_align, remembered_align,
        "zgc::vaddr (Z_OBJECT_ALIGNMENT) and zgc::remembered (Z_REMSET_GRAIN_BYTES) \
         disagree on the object grid"
    );

    // -- Page geometry: page.rs vs forwarding.rs ---------------------------
    assert!(
        page::ZPAGE_DEFAULT_SMALL <= page::ZPAGE_DEFAULT_MEDIUM,
        "zgc::page's small page must not exceed its medium page"
    );
    assert!(
        (page::ZPAGE_DEFAULT_MEDIUM as u64) <= forwarding::ZFWD_MAX_FROM_OFFSET + forwarding_align,
        "zgc::page's medium page ({} B) does not fit zgc::forwarding's \
         {}-bit from-offset field (max {:#x} B)",
        page::ZPAGE_DEFAULT_MEDIUM,
        forwarding::ZFWD_FROM_BITS,
        forwarding::ZFWD_MAX_FROM_OFFSET,
    );
    assert_eq!(
        page::ZPAGE_DEFAULT_GRANULE,
        page::ZPAGE_DEFAULT_SMALL,
        "zgc::page's granule and small page have drifted apart; \
         forwarding's from-offset budget was derived assuming they match \
         OpenJDK's 2 MiB"
    );

    // -- Address widths: vaddr vs forwarding -------------------------------
    assert_eq!(vaddr::Z_METADATA_SHIFT, vaddr::Z_OFFSET_BITS);
    assert_eq!(vaddr::Z_MAX_HEAP_SIZE, 1u64 << vaddr::Z_OFFSET_BITS);
    assert_eq!(vaddr::Z_OFFSET_MASK, vaddr::Z_MAX_HEAP_SIZE - 1);
    assert!(
        forwarding::ZFWD_MAX_PAYLOAD >= vaddr::Z_MAX_HEAP_SIZE - forwarding_align,
        "zgc::forwarding's to-address field ({:#x}) cannot reach the top of \
         zgc::vaddr's heap ({:#x})",
        forwarding::ZFWD_MAX_PAYLOAD,
        vaddr::Z_MAX_HEAP_SIZE,
    );
    // The three fields of a forwarding entry must exactly tile a u64.
    assert_eq!(
        1 + forwarding::ZFWD_TO_BITS + forwarding::ZFWD_FROM_BITS,
        64,
        "zgc::forwarding's entry fields no longer tile a u64"
    );
    assert_eq!(forwarding::ZFWD_TO_SHIFT, forwarding::ZFWD_FROM_BITS);
    assert_eq!(
        forwarding::ZFWD_OCCUPIED_BIT,
        1u64 << 63,
        "zgc::forwarding's occupancy tag and zgc::vaddr's Z_COLORED_TAG are the \
         same bit (63). That is only safe while a forwarding ENTRY is never \
         stored in a reference slot; if it ever is, an occupied entry would \
         read as a colored word."
    );
    assert_eq!(forwarding::ZFWD_OCCUPIED_BIT, vaddr::Z_COLORED_TAG);

    // -- Metadata bit layout, stated once ----------------------------------
    assert_eq!(vaddr::Z_METADATA_BITS, 4);
    assert_eq!(
        vaddr::Z_MARKED0 | vaddr::Z_MARKED1 | vaddr::Z_REMAPPED | vaddr::Z_FINALIZABLE,
        vaddr::Z_METADATA_MASK,
        "zgc::vaddr's four colours must exactly cover Z_METADATA_MASK"
    );
    assert_eq!(
        vaddr::Z_METADATA_MASK & vaddr::Z_OFFSET_MASK,
        0,
        "zgc::vaddr's metadata bits overlap its offset field"
    );
    assert_eq!(
        vaddr::Z_METADATA_MASK & vaddr::Z_COLORED_TAG,
        0,
        "zgc::vaddr's tag bit overlaps a colour bit"
    );
}

/// PINS: that `barrier.rs` re-exports `vaddr`'s constants rather than
/// re-declaring them.
///
/// `barrier.rs` shipped with **copies** of these constants taken from the
/// `zgc.rs` simulation, which uses different bit positions. The copies have
/// been replaced by `pub use super::vaddr::{…}`, but a future edit could
/// re-introduce a local `const` with the same name and nothing would notice —
/// the fast path would simply start classifying correctly-coloured words as
/// bad. This test is the tripwire.
#[test]
fn barrier_reexports_are_bit_identical_to_vaddrs_definitions() {
    assert_eq!(
        barrier::Z_MARKED0,
        vaddr::Z_MARKED0,
        "barrier vs vaddr: Z_MARKED0"
    );
    assert_eq!(
        barrier::Z_MARKED1,
        vaddr::Z_MARKED1,
        "barrier vs vaddr: Z_MARKED1"
    );
    assert_eq!(
        barrier::Z_REMAPPED,
        vaddr::Z_REMAPPED,
        "barrier vs vaddr: Z_REMAPPED"
    );
    assert_eq!(
        barrier::Z_FINALIZABLE,
        vaddr::Z_FINALIZABLE,
        "barrier vs vaddr: Z_FINALIZABLE"
    );
    assert_eq!(
        barrier::Z_METADATA_MASK,
        vaddr::Z_METADATA_MASK,
        "barrier vs vaddr: Z_METADATA_MASK"
    );
    assert_eq!(
        barrier::Z_METADATA_SHIFT,
        vaddr::Z_METADATA_SHIFT,
        "barrier vs vaddr: Z_METADATA_SHIFT"
    );
    assert_eq!(
        barrier::Z_METADATA_BITS,
        vaddr::Z_METADATA_BITS,
        "barrier vs vaddr: Z_METADATA_BITS"
    );
    assert_eq!(
        barrier::Z_OFFSET_BITS,
        vaddr::Z_OFFSET_BITS,
        "barrier vs vaddr: Z_OFFSET_BITS"
    );
    assert_eq!(
        barrier::Z_OFFSET_MASK,
        vaddr::Z_OFFSET_MASK,
        "barrier vs vaddr: Z_OFFSET_MASK"
    );
    assert_eq!(
        barrier::Z_COLORED_TAG,
        vaddr::Z_COLORED_TAG,
        "barrier vs vaddr: Z_COLORED_TAG"
    );
    assert_eq!(barrier::Z_NULL, vaddr::Z_NULL, "barrier vs vaddr: Z_NULL");

    // And the derived masks must agree term for term with the real mask type,
    // in every phase, not just at the default.
    let mask = vaddr::ZGoodMask::new();
    for _ in 0..3 {
        for word in [
            vaddr::color(0x1000, vaddr::ZColor::Marked0),
            vaddr::color(0x1000, vaddr::ZColor::Marked1),
            vaddr::color(0x1000, vaddr::ZColor::Remapped),
            vaddr::color(0x1000, vaddr::ZColor::Finalizable),
        ] {
            let by_vaddr = mask.is_good(word);
            let by_barrier = matches!(
                barrier::classify(word, mask.good()),
                barrier::ZFastPath::Good(_)
            );
            assert_eq!(
                by_vaddr,
                by_barrier,
                "vaddr::ZGoodMask::is_good and barrier::classify disagree about \
                 {word:#x} in phase {:?}",
                mask.phase()
            );
        }
        mask.flip_to_mark();
        mask.set_mark_complete();
        mask.flip_to_remap();
    }
}

/// PINS: `remembered.rs` keys its tables by the SAME `u64` page id every other
/// ZGC module uses — no narrowing anywhere.
///
/// This test used to assert the opposite. `page::ZPageReal::id`,
/// `forwarding::PageCandidate::page_id`, `forwarding::ZForwardingRegistry`,
/// `generation::ZScopeEntry::page_id` and `generation::ZGenerationalHeap::
/// page_generation` were all `u64` while `remembered::ZRememberedSetTable` and
/// `ZRememberedSet::page_id` were `u32`, so every bridge had to narrow.
/// `ZPageAllocator`'s id counter is monotonic and never reused downward, so a
/// long-running VM would eventually hand out ids above `u32::MAX`, past which
/// every `register_old_page` aliased onto a wrong page's remembered set — a
/// silently wrong root set, i.e. live objects collected with no error.
///
/// `remembered.rs` was widened to `u64` on 2026-08-07. The test now pins the
/// resolution rather than the defect: distinct `u64` ids must stay distinct
/// keys, and an id above `u32::MAX` must round-trip rather than alias.
#[test]
fn page_ids_reach_the_remembered_set_table_without_narrowing() {
    let allocator = small_allocator();
    let heap = generational_heap(Arc::clone(&allocator), 64);

    for _ in 0..64 {
        heap.allocate_young(64, 8).expect("young allocation");
    }
    heap.allocate_old(64, 8).expect("old allocation");

    let table = remembered::ZRememberedSetTable::new();
    let mut seen: HashSet<u64> = HashSet::new();
    for page in allocator.pages() {
        let id = page.id();
        assert!(
            seen.insert(id),
            "ZPageAllocator handed out page id {id} twice; the remembered set              would silently merge two pages' root sets"
        );
        table.register_old_page(id, page.size());
    }

    assert_eq!(
        table.len(),
        seen.len(),
        "one remembered set per page; a collision would silently merge two          pages' root sets"
    );

    // The case the old u32 key could not represent at all: an id past
    // u32::MAX must key its own set, not alias onto `id & 0xFFFF_FFFF`.
    let low = 7u64;
    let high = (1u64 << 32) | 7;
    table.register_old_page(low, 4096);
    table.register_old_page(high, 4096);
    assert!(
        table.get(low).is_some() && table.get(high).is_some(),
        "an id above u32::MAX must key its own remembered set"
    );

    for page in allocator.pages() {
        let set = table
            .get(page.id())
            .expect("every registered page must be retrievable by its narrowed id");
        assert_eq!(
            set.page_size(),
            page.size(),
            "remembered.rs recorded a different page size than page.rs reports \
             for page {}",
            page.id()
        );
        assert_eq!(u64::from(set.page_id()), page.id());
    }
}

/// PINS: that `page.rs`'s allocator statistics stay internally consistent while
/// `generation.rs` drives it, so a report assembled from both modules adds up.
///
/// `ZGenerationalStats` (generation.rs) and `ZPageAllocatorStats` (page.rs)
/// count overlapping things from two different sides. A collector report that
/// mixes them — which any real one will — needs them to agree.
#[test]
fn generation_and_page_allocator_statistics_describe_the_same_heap() {
    let allocator = small_allocator();
    let heap = generational_heap(Arc::clone(&allocator), 64);

    for _ in 0..100 {
        heap.allocate_young(64, 8).expect("young allocation");
    }
    heap.allocate_old(96, 8).expect("old allocation");
    allocator.retire_shared_pages();

    let gen_stats = heap.stats();
    let page_stats = allocator.stats();

    assert_eq!(
        gen_stats.young_pages + gen_stats.old_pages,
        allocator.pages().len(),
        "generation.rs claims {} young + {} old pages, but page.rs owns {}",
        gen_stats.young_pages,
        gen_stats.old_pages,
        allocator.pages().len(),
    );
    assert_eq!(
        gen_stats.young_used + gen_stats.old_used,
        page_stats.used,
        "generation.rs's per-generation `used` totals must sum to page.rs's \
         allocator-wide `used`"
    );
    assert_eq!(
        gen_stats.young_capacity + gen_stats.old_capacity,
        allocator.max_capacity(),
        "the generational budget split must partition page.rs's max_capacity"
    );
    assert!(
        page_stats.committed <= page_stats.max_capacity,
        "page.rs committed more than its budget"
    );
    assert!(gen_stats.young_used > 0);
    assert!(gen_stats.old_used > 0);
}
