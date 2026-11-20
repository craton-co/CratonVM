use super::{gci_i42_mx_explicit_gc_flags, gci_i42_mx_hotspot_heap_sizes};
use crate::config::ExplicitGcPolicy;

#[test]
fn gci_i42_mx_explicit_gc_flags_follow_the_policy() {
    assert_eq!(gci_i42_mx_explicit_gc_flags(ExplicitGcPolicy::Full, false), (false, false));
    assert_eq!(gci_i42_mx_explicit_gc_flags(ExplicitGcPolicy::Disabled, false), (true, false));
    assert_eq!(gci_i42_mx_explicit_gc_flags(ExplicitGcPolicy::Concurrent, false), (false, true));
    assert_eq!(
        gci_i42_mx_explicit_gc_flags(ExplicitGcPolicy::from_hotspot_flags(None, Some(true)), true),
        (false, true)
    );
}

/// gci i43/lf (`gci-i42mx-...` item 8): `-XX:+DisableExplicitGC
/// -XX:+ExplicitGCInvokesConcurrent` reports both flags on, as HotSpot.
#[test]
fn gci_i43_lf_both_explicit_gc_flags_report_the_concurrent_flag() {
    let both = ExplicitGcPolicy::from_hotspot_flags(Some(true), Some(true));
    assert_eq!(both, ExplicitGcPolicy::Disabled);
    assert_eq!(gci_i42_mx_explicit_gc_flags(both, true), (true, true));
    assert_eq!(gci_i42_mx_explicit_gc_flags(ExplicitGcPolicy::Disabled, false), (true, false));
    // An inconsistent embedder config never reports a concurrent `Full`.
    assert_eq!(gci_i42_mx_explicit_gc_flags(ExplicitGcPolicy::Full, true), (false, false));
}

/// gci i43/lf: the TLAB configuration answer is HotSpot's defaults until
/// a flag is given.
#[test]
fn gci_i43_lf_tlab_configuration_reads_the_flags() {
    use crate::config::TlabFlags;
    assert_eq!(super::gci_i43_lf_tlab_configuration(&TlabFlags::default()), (true, 2048, 64));
    let set = TlabFlags {
        use_tlab: Some(false),
        min_tlab_size: Some(4096),
        refill_waste_fraction: Some(16),
        ..TlabFlags::default()
    };
    assert_eq!(super::gci_i43_lf_tlab_configuration(&set), (false, 4096, 16));
}

#[test]
fn gci_i42_mx_heap_sizes_are_the_gc_init_figures() {
    const M: usize = 1024 * 1024;
    let m = |n: usize| (n * M) as i64;
    // `-Xms256m -Xmx256m`: min = initial = max (the row shape).
    assert_eq!(gci_i42_mx_hotspot_heap_sizes(Some(256 * M), 256 * M, 256 * M), [m(256), m(256), m(256)]);
    // `-Xmx256m` alone with a 16 MiB initial: HotSpot's `min(8M, initial)`.
    assert_eq!(gci_i42_mx_hotspot_heap_sizes(None, 16 * M, 256 * M), [m(8), m(16), m(256)]);
    // An initial size above `-Xmx` is clamped, and so the minimum.
    assert_eq!(gci_i42_mx_hotspot_heap_sizes(None, 16 * M, 6 * M), [m(6), m(6), m(6)]);
}
