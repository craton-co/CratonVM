use super::*;
use crate::config::{GcAlgorithm, VmConfig};

/// Item 4: a G1 humongous request collects on the humongous site, every
/// other request (and an unknown size, `0`) on `vm-exec`, on every backend.
#[test]
fn gci_i13oa_the_native_door_site_follows_the_request_size() {
    assert_eq!(native_door_site_for(G1_HUMONGOUS_ALLOC_SITE), G1_HUMONGOUS_ALLOC_SITE);
    assert_eq!(native_door_site_for("gc-alloc-array"), "vm-exec");
    let g1 = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::G1,
        ..VmConfig::default()
    });
    let region = match &g1.mem.heap {
        _ if g1.mem.heap.is_g1() => g1.mem.heap.region_census().map(|c| c.region_size).unwrap_or(0),
        _ => panic!("a G1 VM"),
    };
    assert_eq!(native_door_site(&g1, region), G1_HUMONGOUS_ALLOC_SITE);
    assert_eq!(native_door_site(&g1, region / 2), "vm-exec");
    assert_eq!(native_door_site(&g1, 0), "vm-exec");
    let gen = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    assert_eq!(native_door_site(&gen, 1 << 30), "vm-exec");
    assert_eq!(
        native_array_footprint(ArrayElementType::Long, 3),
        cratonvm_gc::heap::ARRAY_DATA_OFFSET + 24
    );
}

/// gci i19/oo (item 4 residual (a)): the native String doors' sized ladder
/// collects a G1 humongous `value` on the humongous site and keeps the
/// caller's site for everything else (every backend, every smaller String,
/// and an unknown footprint `0`).
#[test]
fn gci_i19oo_the_native_string_door_site_follows_the_value_footprint() {
    assert_eq!(
        native_sized_site_for(G1_HUMONGOUS_ALLOC_SITE, "native-gc-safe-string"),
        G1_HUMONGOUS_ALLOC_SITE
    );
    assert_eq!(
        native_sized_site_for("gc-alloc-array", "native-gc-safe-string"),
        "native-gc-safe-string"
    );
    let g1 = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::G1,
        ..VmConfig::default()
    });
    let region = match &g1.mem.heap {
        _ if g1.mem.heap.is_g1() => g1.mem.heap.region_census().map(|c| c.region_size).unwrap_or(0),
        _ => panic!("a G1 VM"),
    };
    let site = "native-gc-safe-string-units";
    // A Latin-1 String of `region` units: a `byte[region]`, humongous.
    let big = native_string_value_footprint(true, region, true);
    assert_eq!(native_sized_door_site(&g1, site, big), G1_HUMONGOUS_ALLOC_SITE);
    // Half as many UTF-16 units weigh the same bytes, humongous too.
    let utf16 = native_string_value_footprint(true, region / 2, false);
    assert_eq!(utf16, big);
    assert_eq!(native_sized_door_site(&g1, site, utf16), G1_HUMONGOUS_ALLOC_SITE);
    // A quarter region of Latin-1: not humongous, the caller's site.
    let small = native_string_value_footprint(true, region / 4, true);
    assert_eq!(native_sized_door_site(&g1, site, small), site);
    assert_eq!(native_sized_door_site(&g1, site, 0), site);
    let gen = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    assert_eq!(native_sized_door_site(&gen, site, 1 << 30), site);    }

/// gci i19/oo: the `value` footprint is the compact layout's -- one byte a
/// unit only when compact strings are on AND every unit fits Latin-1.
#[test]
fn gci_i19oo_the_string_value_footprint_follows_the_coder() {
    let header = cratonvm_gc::heap::ARRAY_DATA_OFFSET;
    assert!(native_string_value_footprint(true, 1000, true) >= header + 1000);
    assert!(native_string_value_footprint(true, 1000, true) < header + 2000);
    assert!(native_string_value_footprint(true, 1000, false) >= header + 2000);
    assert!(native_string_value_footprint(false, 1000, true) >= header + 2000);
    assert_eq!(
        native_string_value_footprint(true, 0, true),
        native_string_value_footprint(false, 0, false)
    );
}

/// Item 2: the rung is skipped only with the switch, an armed remark and
/// nothing young left -- and the young check is asked only then.
#[test]
fn gci_i13oa_the_soft_rung_is_skipped_only_on_full_evidence() {
    let asked = std::cell::Cell::new(0);
    let young_left = |answer: bool| {
        asked.set(asked.get() + 1);
        answer
    };
    assert!(soft_rung_skippable(true, true, || young_left(true)));
    assert!(!soft_rung_skippable(true, true, || young_left(false)));
    assert_eq!(asked.get(), 2);
    assert!(!soft_rung_skippable(false, true, || young_left(true)));
    assert!(!soft_rung_skippable(true, false, || young_left(true)));
    assert_eq!(asked.get(), 2, "no young check without the switch and a remark");
}

