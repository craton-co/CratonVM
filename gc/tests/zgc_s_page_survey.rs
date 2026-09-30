// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! C0 stage 0 — "one reservation, two views" — against the crate's **public**
//! page API.
//!
//! `docs/feature-designs/zgc-roadmap-20260920.md` phase 1 schedules C0 first
//! on the grounds that it is the only item in the whole programme that is pure
//! win with no flag and no behaviour change. The reason it can make that claim
//! is the refusal asserted here: an allocator built by
//! [`ZPageAllocator::over_reservation`] describes a window it does not own and
//! **cannot allocate out of it**. If that ever stops being true, stage 0 stops
//! being free — two allocators would be bumping into one window, and the first
//! object written into a page carved here would land on a live Java object
//! with no error on either side.
//!
//! These are integration tests rather than `#[cfg(test)]` units for the same
//! reason `zgc_c_page_allocator_exhaustion.rs` is: the assertions are made
//! against the surface a future page-backed `ZgcRealHeap` would be built on,
//! and `ZgcRealHeap` is not in this crate's `zgc::page` module.
//!
//! The grid-indexing half of the story needs `ZPageReal::view`, which is
//! `pub(crate)` on purpose, so it lives in `page.rs`'s own suite.

use cratonvm_gc::zgc::page::{
    page_survey_enabled, ZPageAllocator, ZPageConfig, ZPageError, ZPageSizeClass,
};

/// A miniature geometry, the same shape as OpenJDK's.
fn test_config() -> ZPageConfig {
    ZPageConfig {
        granule_size: 4096,
        small_page_size: 8192,
        medium_page_size: 65536,
        small_object_limit: 1024,
        medium_object_limit: 8192,
        max_capacity: 4096 * 64,
    }
}

/// Real owned bytes standing in for `ZgcRealHeap`'s arena. The `Vec` is the
/// owner; the survey only ever describes it.
fn window(granules: usize) -> (Vec<u8>, usize, usize) {
    let c = test_config();
    let backing = vec![0u8; c.granule_size * (granules + 1)];
    let raw = backing.as_ptr() as usize;
    let base = (raw + c.granule_size - 1) & !(c.granule_size - 1);
    (backing, base, c.granule_size * granules)
}

/// The survey's geometry comes from the **window**, not from
/// `config.max_capacity`. Surveying a heap that is already running and then
/// disagreeing with it about how big it is would describe granules that are
/// not there — which is `page_for` answering with a page that does not exist.
#[test]
fn a_survey_describes_the_window_it_was_given() {
    let (_backing, base, len) = window(8);
    let alloc = ZPageAllocator::over_reservation(base, len, test_config())
        .expect("a granule-aligned window must survey");

    assert!(alloc.is_survey_only());
    assert!(!alloc.owns_reservation());
    assert!(alloc.base_is_granule_aligned());
    assert_eq!(alloc.base(), base);
    assert_eq!(alloc.max_capacity(), len);
    assert_eq!(alloc.end(), base + len);
    assert!(alloc.in_reserved_range(base));
    assert!(!alloc.in_reserved_range(base + len));
    assert!(alloc.pages().is_empty());
    assert_eq!(alloc.survey_stats().grid_refreshes, 0);
}

/// An owning allocator is unchanged by any of this, and says which it is.
#[test]
fn new_still_owns_its_reservation() {
    let alloc = ZPageAllocator::new(test_config()).expect("test geometry must validate");
    assert!(alloc.owns_reservation());
    assert!(!alloc.is_survey_only());
    assert!(alloc.base_is_granule_aligned());
    assert!(alloc.alloc_page(ZPageSizeClass::Small, 0).is_ok());
    assert_eq!(
        alloc.survey_stats().grid_refreshes,
        0,
        "an owning allocator is never surveyed, so its survey counters stay zero and \
         cannot be mistaken for engagement",
    );
}

/// **The refusal that makes stage 0 free.**
///
/// Written twice, as every other guard in `page.rs` is: in a debug build the
/// `debug_assert!` fires first, which is the louder and preferred outcome; in
/// release the call returns `ZPageError::SurveyOnly` and nothing is handed
/// out. Both are enforcement.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "alloc_page on a SURVEY")]
fn a_survey_cannot_allocate() {
    let (_backing, base, len) = window(8);
    let alloc = ZPageAllocator::over_reservation(base, len, test_config()).unwrap();
    let _ = alloc.alloc_page(ZPageSizeClass::Small, 0);
}

#[cfg(not(debug_assertions))]
#[test]
fn a_survey_cannot_allocate() {
    let (_backing, base, len) = window(8);
    let alloc = ZPageAllocator::over_reservation(base, len, test_config()).unwrap();

    // `SurveyOnly`, and deliberately not `OutOfCapacity` or `Fragmented`:
    // those two read as heap pressure and invite a retry, a bigger budget or a
    // cache flush. This one cannot ever succeed, whatever the caller does.
    assert!(matches!(
        alloc.alloc_page(ZPageSizeClass::Small, 0),
        Err(ZPageError::SurveyOnly),
    ));
    assert!(matches!(
        alloc.alloc_object(64, 8),
        Err(ZPageError::SurveyOnly),
    ));
    assert!(matches!(
        alloc.alloc_object(test_config().medium_object_limit + 1, 8),
        Err(ZPageError::SurveyOnly),
    ));

    let stats = alloc.stats();
    assert_eq!(stats.committed, 0);
    assert_eq!(stats.used, 0);
    assert_eq!(
        stats.small_pages + stats.medium_pages + stats.large_pages,
        0
    );
    assert_eq!(
        stats.free_granules, 0,
        "a survey owns no free granule: every byte in the window belongs to the arena",
    );
    assert!(alloc.pages().is_empty());
}

/// A window shorter than one granule is not a heap. Refused rather than
/// rounded up to a granule that is not there — the same fail-safe direction
/// `ZPageAllocator::new` takes with an odd `max_capacity`.
#[test]
fn a_window_below_one_granule_is_refused() {
    let c = test_config();
    assert!(matches!(
        ZPageAllocator::over_reservation(c.granule_size, c.granule_size - 1, c),
        Err(ZPageError::InvalidConfig(_)),
    ));
}

/// The window is truncated **down** to whole granules, so the survey never
/// claims to index a partial granule at the top.
#[test]
fn a_ragged_window_truncates_down() {
    let c = test_config();
    let (_backing, base, len) = window(4);
    let alloc = ZPageAllocator::over_reservation(base, len + c.granule_size / 2, c).unwrap();
    assert_eq!(alloc.max_capacity(), len);
    assert_eq!(alloc.table().granule_count(), 4);
}

/// A default-OFF knob that reads as on is the specific mistake this repo has
/// made twice, once inside this very round (`zgc-page-evac`). The declaration
/// that matches this assertion is `off_word: None` in
/// `types/src/flag_groups.rs`.
#[test]
fn the_survey_flag_is_default_off() {
    if std::env::var_os("CRATONVM_ZGC_PAGE_SURVEY").is_none() {
        assert!(
            !page_survey_enabled(),
            "CRATONVM_ZGC_PAGE_SURVEY must be OFF when unset",
        );
    }
}
