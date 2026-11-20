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
