// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
mod tests;
