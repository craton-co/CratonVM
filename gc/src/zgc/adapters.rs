// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The one seam between the ZGC submodules that a production path uses:
//! `page::ZPageReal` → `forwarding::PageCandidate`.
//!
//! # What this file is now
//!
//! Three functions. [`page_candidate`] and [`page_candidates`] project a page
//! into the four integers `forwarding`'s relocation-set selector consumes, and
//! [`zfwd_size_class_index`] is the size-class mapping they are built on.
//! `zgc.rs`'s logical-page split calls `page_candidates`; nothing else here is
//! public surface waiting for a consumer, because there is nothing else here.
//!
//! # What it was, and why six other adapters were retired (wave 4, 2026-09-21)
//!
//! The modules under `crate::zgc::` were written in parallel by authors who
//! could not compile against one another's work, so each declares its own
//! traits and plain-data structs rather than importing a sibling's types. This
//! file was the glue, and its own rule said what to do about that:
//!
//! > Where two modules model the same thing with two different types, the
//! > long-term fix is for **the two modules to agree on one type** and for the
//! > adapter to be deleted — not for this file to grow.
//!
//! Six items reached that fix by two different routes and were deleted:
//!
//! * `zpage_size_class_of_index` — the inverse of [`zfwd_size_class_index`].
//!   It had no caller anywhere, including in this module's own tests once the
//!   `page_candidate` assertions were written against the page's class rather
//!   than round-tripped through it. Note that the forward mapping is **not**
//!   dead and never was: [`page_candidate`] calls it, which the wave-3
//!   removal plan in this header got wrong.
//! * `ZHeapGenerationContext` (seam 3) — a `ZGenerationContext` over
//!   `ZGenerationalHeap`. No production caller, and its own header warned that
//!   it costs three locks and a refcount round trip per reference store, i.e.
//!   that it must never go under the barrier it was written for.
//! * `ZOldSlotReader`, `ZMapSlotReader`, `ZRemsetBuffer`,
//!   `ZTableRememberedSetView`, `old_page_bases`, `register_old_pages`
//!   (seam 4) — **superseded, not merely unused.** That seam existed because
//!   `remembered` "exposes no API that performs [the old-slot read]", so the
//!   consumer had to supply a `page_id → base` map out of band, and a missing
//!   entry in it was a silently dropped root. `remembered.rs` has since grown
//!   exactly the method that header asked it for
//!   (`ZRememberedSetTable::iterate_slot_addresses`, whose doc says so in as
//!   many words), resolving the page-base decision *inside* the module from
//!   the base the registrar supplied. As of wave 4 the shipping heap consumes
//!   it: `ZgcRealHeap::young_extra_roots` scans its cards through that call and
//!   counts the refusals (`remset_pages_without_base`). The deletion condition
//!   the seam stated for itself has been met.
//!
//! ## The colour conversion was not deleted with it
//!
//! Wave 3 extended `ZTableRememberedSetView` with the mandatory colour/offset
//! conversion (`vaddr::root_address_for_slot_word`), and the round summary
//! recorded that as a reason not to delete the type. The conversion is a
//! `vaddr` function and stays in `vaddr`; what went was one call site that no
//! production path reached. It is pinned independently by
//! `gc/tests/zgc_p_slot_word_conversion.rs` — all three arms, including the
//! platform-dependent one where an unconverted 42-bit offset lands inside a
//! Windows heap and hides the bug that drops every edge on Linux. Retiring a
//! dead caller must not retire the behaviour, which is what that test is for.
//!
//! # Policy: none
//!
//! Nothing here decides anything. Every function is a projection of data one
//! module already owns into the shape another module already asks for. If a
//! future edit finds itself choosing a threshold, a default, or a fallback
//! behaviour, that is the signal that the two modules disagree and the choice
//! belongs in one of them, with a test, not here.
//!
//! **Adding an adapter here needs a production caller in the same commit.**
//! That rule is the whole lesson of the six deletions above: every one of them
//! was written against a consumer that was going to exist, and the module they
//! bridged solved the problem itself before the consumer arrived.
//!
//! # Address domains
//!
//! Three different "addresses" appear around these modules and they are NOT
//! interchangeable:
//!
//! | domain | width | who speaks it |
//! |---|---|---|
//! | **uncolored machine address** | `usize` (or `u64`) | `page` (`base`/`end`/`alloc`), `generation` (`ZGenerationAllocation::address`, `ZGenerationMarker`'s roots), `remembered`'s `ZGenerationContext` and `ZStoreBarrier` |
//! | **42-bit heap offset** | `u64`, masked by `vaddr::Z_OFFSET_MASK` | `vaddr`, `barrier`'s masks, `forwarding`'s `to` field |
//! | **page-relative offset** | `usize`, grain-aligned | `remembered`'s bitmaps (`iterate`, `remember`, `is_remembered`) |
//!
//! **What survives in this file carries no address at all** — `PageCandidate`
//! is four integers and identifies its page by id — so the domain question does
//! not arise here any more. It arises in `remembered` and `vaddr`, which is
//! where the conversion now lives. If a future adapter does carry an address,
//! name its domain in the signature's doc, as everything here used to.
//!
//! # Page-id width
//!
//! `page`, `forwarding` and `generation` key pages by `u64`, and so does
//! `remembered` (it used to key them by `u32`, an aliasing bug on a timer:
//! `ZPageAllocator`'s ids are monotonic and never recycled downward, so a
//! long-running VM reaches `u32::MAX`). Everything in this file is written
//! against `u64` page ids.

use std::sync::Arc;

use crate::zgc::forwarding::{
    PageCandidate, ZFWD_SIZE_CLASS_LARGE, ZFWD_SIZE_CLASS_MEDIUM, ZFWD_SIZE_CLASS_SMALL,
};
use crate::zgc::page::{ZPageReal, ZPageSizeClass};

// ===========================================================================
// Seam 1: page::ZPageSizeClass  <->  forwarding::ZFWD_SIZE_CLASS_*
// ===========================================================================

/// `page`'s size-class **enum** → `forwarding`'s size-class **`u8`**.
///
/// * Direction: `page` → `forwarding`. Total, infallible, no units involved.
/// * Failure cases: none. The match is exhaustive by construction, so adding a
///   fourth `ZPageSizeClass` variant is a compile error here rather than a
///   silent mis-classification at a call site.
///
/// # Why this is worth a named function
///
/// Getting `Large` wrong is not a cosmetic bug. `forwarding`'s relocation
/// policy has exactly one structural exclusion —
/// `ZRelocationPolicy::relocate_large_pages`, `false` by default — and it is
/// implemented as `PageCandidate::is_large()`, i.e. as a comparison against
/// [`ZFWD_SIZE_CLASS_LARGE`]. A caller that hand-rolls this mapping and
/// mis-numbers `Large` **admits large pages to the relocation set**, which is
/// the single most expensive copy in the heap and buys nothing (a large page
/// holds one object sized to fit, so there is no fragmentation to recover).
/// `page::ZPageReal::is_relocation_candidate` excludes `Large` independently,
/// so the two gates are meant to agree; this function is what makes them
/// agree.
///
/// # The real fix
///
/// Delete this function by making the two modules share one type. `page`'s
/// enum is not `#[repr(u8)]` and carries no discriminants, so today there is
/// no ABI-level identity to lean on and the match is the honest bridge.
#[inline]
pub fn zfwd_size_class_index(class: ZPageSizeClass) -> u8 {
    match class {
        ZPageSizeClass::Small => ZFWD_SIZE_CLASS_SMALL,
        ZPageSizeClass::Medium => ZFWD_SIZE_CLASS_MEDIUM,
        ZPageSizeClass::Large => ZFWD_SIZE_CLASS_LARGE,
    }
}

// ===========================================================================
// Seam 2: page::ZPageReal  ->  forwarding::PageCandidate
// ===========================================================================

/// A live [`ZPageReal`] → a [`PageCandidate`], measuring capacity as the
/// **allocated extent** ([`ZPageReal::relocation_capacity_bytes`], i.e. the
/// bump cursor).
///
/// * Direction: `page` → `forwarding`. Total, infallible.
/// * Units: bytes throughout. No address is carried — `PageCandidate` is four
///   integers and identifies the page by id, so no address domain is involved.
/// * Snapshot semantics: `live_bytes` is an `AtomicUsize` read `Relaxed` by
///   `page`, so the candidate is a **snapshot at call time**. Build the whole
///   candidate vector at one point in the cycle (after mark, before selection)
///   rather than re-reading pages during selection.
///
/// # The denominator: decided elsewhere, not chosen here
///
/// `page` and `forwarding` once divided by different denominators, so this
/// file shipped *two* mappings and refused to pick. **`page` has since
/// decided, and the allocated extent won.** The argument is not restated here:
/// it is the dated 2026-08-07 note on
/// [`ZPageReal::relocation_capacity_bytes`], which is the specification for
/// the `capacity_bytes` line below. The page-span measure (`ZPageReal::size()`)
/// is not a supported alternative and its mapping has been deleted — do not
/// reintroduce it.
///
/// With one denominator the two modules' arithmetic is identical on the same
/// page:
///
/// ```text
/// candidate.garbage_bytes()   == page.garbage_bytes()
/// candidate.live_occupancy()  == page.live_ratio()      (for used() > 0)
/// ```
///
/// so `forwarding`'s `max_live_occupancy` and `page`'s
/// `is_relocation_candidate(live_ratio_threshold)` mean the same thing and the
/// two independent gates cannot disagree.
///
/// # And what it costs
///
/// `PageCandidate::live_occupancy` returns `1.0` when `capacity_bytes == 0`,
/// and `ZRelocationSet::select` skips `capacity_bytes == 0` outright. A page
/// with `used() == 0` (freshly reset, or handed out and never allocated into)
/// therefore drops out of selection. That is the right outcome — an empty page
/// must be *freed*, not evacuated — but no module currently performs that
/// sweep. That gap is real and is reported rather than patched.
pub fn page_candidate(page: &ZPageReal) -> PageCandidate {
    PageCandidate {
        page_id: page.id(),
        live_bytes: page.live_bytes(),
        capacity_bytes: page.relocation_capacity_bytes(),
        size_class_index: zfwd_size_class_index(page.size_class()),
    }
}

/// Map a slice of pages through [`page_candidate`].
///
/// A convenience for the common `ZPageAllocator::pages()` /
/// `ZOldGeneration::snapshot()` → `ZRelocationSet::select` path. There is one
/// capacity measure and therefore no choice to make at the call site; for why,
/// see the dated note on [`ZPageReal::relocation_capacity_bytes`].
pub fn page_candidates(pages: &[Arc<ZPageReal>]) -> Vec<PageCandidate> {
    pages.iter().map(|p| page_candidate(p.as_ref())).collect()
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    use crate::zgc::forwarding::{ZRelocationPolicy, ZRelocationSet};
    use crate::zgc::page::{ZPageAllocator, ZPageConfig};

    /// The 1/512-scale geometry `page.rs`'s own tests and
    /// `gc/tests/zgc_module_integration.rs` use: 4 KiB granule, 8 KiB small
    /// page, 64 KiB medium page, 256 KiB budget. Nothing here commits
    /// hundreds of MiB.
    fn small_geometry() -> ZPageConfig {
        ZPageConfig {
            granule_size: 4096,
            small_page_size: 8192,
            medium_page_size: 65536,
            small_object_limit: 8192 / 8,
            medium_object_limit: 65536 / 8,
            max_capacity: 4096 * 64,
        }
    }

    fn small_allocator() -> Arc<ZPageAllocator> {
        Arc::new(ZPageAllocator::new(small_geometry()).expect("scaled test geometry must validate"))
    }

    // -- Seam 1 -------------------------------------------------------------

    /// PINS: that the enum→u8 mapping is a bijection onto the three declared
    /// constants. A drift here silently re-labels every page handed to
    /// relocation-set selection.
    #[test]
    fn size_class_maps_injectively_onto_the_three_forwarding_constants() {
        assert_eq!(
            zfwd_size_class_index(ZPageSizeClass::Small),
            ZFWD_SIZE_CLASS_SMALL
        );
        assert_eq!(
            zfwd_size_class_index(ZPageSizeClass::Medium),
            ZFWD_SIZE_CLASS_MEDIUM
        );
        assert_eq!(
            zfwd_size_class_index(ZPageSizeClass::Large),
            ZFWD_SIZE_CLASS_LARGE
        );

        // The three indices must be distinct, or two classes alias. With the
        // inverse retired this is what carries injectivity: a round trip
        // through a hand-written inverse would have proved the two functions
        // agree with each other, not that the three constants differ.
        assert_ne!(ZFWD_SIZE_CLASS_SMALL, ZFWD_SIZE_CLASS_MEDIUM);
        assert_ne!(ZFWD_SIZE_CLASS_MEDIUM, ZFWD_SIZE_CLASS_LARGE);
        assert_ne!(ZFWD_SIZE_CLASS_SMALL, ZFWD_SIZE_CLASS_LARGE);

        // And that the mapping is total over the enum: adding a fourth variant
        // must be a compile error here, which the exhaustive match in
        // `zfwd_size_class_index` is. This loop asserts nothing a reader could
        // not derive; it exists so the enumeration is written down beside the
        // three constants it maps onto.
        for class in [
            ZPageSizeClass::Small,
            ZPageSizeClass::Medium,
            ZPageSizeClass::Large,
        ] {
            let index = zfwd_size_class_index(class);
            assert!(
                matches!(
                    index,
                    ZFWD_SIZE_CLASS_SMALL | ZFWD_SIZE_CLASS_MEDIUM | ZFWD_SIZE_CLASS_LARGE
                ),
                "{} maps to {index}, which is not one of forwarding's declared classes",
                class.as_str(),
            );
        }
    }

    /// PINS: that `Large` maps to the constant `ZRelocationPolicy` actually
    /// excludes — asserted through `ZRelocationSet::select`, not by comparing
    /// integers.
    ///
    /// This is the assertion that matters. Two candidates differing in
    /// **nothing but the size class** must select differently: the Small one
    /// in, the Large one out. If the mapping ever numbers `Large` as something
    /// the policy does not exclude, the large page is admitted to the
    /// relocation set — the single most expensive copy in the heap, for zero
    /// reclaim.
    #[test]
    fn large_maps_to_the_index_the_relocation_policy_excludes() {
        // Deliberately attractive on every other axis: 1% live occupancy, far
        // under the 25% cutoff and far under the 64 MiB budget. The ONLY
        // reason to reject it is the size class.
        let make = |class: ZPageSizeClass, id: u64| PageCandidate {
            page_id: id,
            live_bytes: 10_000,
            capacity_bytes: 1_000_000,
            size_class_index: zfwd_size_class_index(class),
        };

        let policy = ZRelocationPolicy::default();
        assert!(
            !policy.relocate_large_pages,
            "the default policy must exclude large pages, or this test proves nothing"
        );

        let small = make(ZPageSizeClass::Small, 1);
        let medium = make(ZPageSizeClass::Medium, 2);
        let large = make(ZPageSizeClass::Large, 3);

        assert!(!small.is_large());
        assert!(!medium.is_large());
        assert!(
            large.is_large(),
            "zfwd_size_class_index(Large) does not produce the value \
             PageCandidate::is_large() tests for"
        );

        let set = ZRelocationSet::select(&[small, medium, large], &policy);
        let ids = set.page_ids();
        assert!(ids.contains(&1), "the small page must be selected");
        assert!(ids.contains(&2), "the medium page must be selected");
        assert!(
            !ids.contains(&3),
            "the LARGE page entered the relocation set: zfwd_size_class_index(Large) \
             is not the constant ZRelocationPolicy excludes"
        );
        assert!(!set.contains(3));
        assert_eq!(set.len(), 2);
    }

    // -- Seam 2 -------------------------------------------------------------

    /// PINS: that a candidate built from a real page carries that page's own
    /// id, live bytes and class, and that its capacity is the allocated extent
    /// — the identity that keeps `forwarding`'s gate and `page`'s gate talking
    /// about the same number.
    #[test]
    fn a_page_candidate_carries_the_real_pages_live_and_capacity_bytes() {
        let allocator = small_allocator();
        let page = allocator
            .alloc_page(ZPageSizeClass::Small, 0)
            .expect("small page");

        // Allocate part of the page and mark part of that live, so `used()`
        // and `size()` differ and the wrong denominator would be visible.
        let object_bytes = 512usize;
        let addr = page.alloc(object_bytes, 8).expect("bump allocation fits");
        assert!(page.contains(addr));
        assert_eq!(page.used(), object_bytes);
        assert!(
            page.used() < page.size(),
            "the fixture must leave the page partly filled, or a capacity taken \
             from size() would be indistinguishable from the right one"
        );
        page.set_live_bytes(128);

        let extent = page_candidate(&page);
        assert_eq!(extent.page_id, page.id());
        assert_eq!(extent.live_bytes, 128);
        assert_eq!(extent.capacity_bytes, page.relocation_capacity_bytes());
        assert_eq!(extent.capacity_bytes, page.used());
        // Asserted against the constant and against the page's own class, not
        // through an inverse mapping: `zpage_size_class_of_index` was retired
        // in wave 4 (it had no caller at all, including here once this line was
        // written the honest way), and asserting a value by round-tripping it
        // through a function in the same file proves only that the two agree.
        assert_eq!(extent.size_class_index, ZFWD_SIZE_CLASS_SMALL);
        assert_eq!(
            extent.size_class_index,
            zfwd_size_class_index(page.size_class())
        );

        // The allocated-extent measure is the one that agrees with `page.rs`'s
        // own arithmetic. This is the identity that makes `forwarding`'s
        // max_live_occupancy and `page`'s live_ratio_threshold mean the same.
        assert_eq!(extent.garbage_bytes(), page.garbage_bytes());
        assert_eq!(extent.live_occupancy(), page.live_ratio());

        // NOTE: this test used to build a second candidate from `page.size()`
        // and `assert_ne!` that the two measures diverge, as a standing report
        // that the modules disagreed. Those assertions are deliberately gone.
        // The divergence is still perfectly reachable — it is not an
        // unreachable corner — but since the denominator was decided (see the
        // dated 2026-08-07 note on `ZPageReal::relocation_capacity_bytes`) it
        // is a *bug to prevent*, not a fact to preserve. Pinning it would pin
        // the wrong measure back into existence. Do not re-add it.

        let batch = page_candidates(&[Arc::clone(&page)]);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0], extent);
    }

    /// PINS: that a real `Large` page reaches `forwarding` labelled as large,
    /// i.e. that the seam-1 mapping survives contact with a page the allocator
    /// actually handed out.
    #[test]
    fn a_real_large_page_reaches_forwarding_labelled_large() {
        let allocator = small_allocator();
        // Larger than the medium object limit, so `class_for` routes it Large.
        let huge = small_geometry().medium_object_limit + 1;
        let page = allocator
            .alloc_page(ZPageSizeClass::Large, huge)
            .expect("large page");
        assert_eq!(page.size_class(), ZPageSizeClass::Large);

        // The Large gate in `ZRelocationPolicy` is `is_large()`, not capacity,
        // so the denominator is irrelevant to what this test pins.
        let candidate = page_candidate(&page);
        assert_eq!(candidate.size_class_index, ZFWD_SIZE_CLASS_LARGE);
        assert!(candidate.is_large());

        let set = ZRelocationSet::select(&[candidate], &ZRelocationPolicy::default());
        assert!(
            set.is_empty(),
            "a real Large page was admitted to the relocation set"
        );
    }
}
