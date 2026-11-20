use super::*;

#[test]
fn find_enclosing_key_prefers_registered_span_over_overlapping_unpinned_span() {
    let mut state = PinCacheState {
        spans: BTreeMap::new(),
        registered_spans: BTreeMap::new(),
        seq: 0,
    };
    state.spans.insert(
        (0x1000, 0x1000),
        SpanEntry {
            refcount: 1,
            registered: true,
            dev_ptr_base: None,
            last_used_seq: 1,
        },
    );
    state.registered_spans.insert(0x1000, 0x1000);
    state.spans.insert(
        (0x1000, 0x2000),
        SpanEntry {
            refcount: 1,
            registered: false,
            dev_ptr_base: None,
            last_used_seq: 2,
        },
    );
    assert_eq!(
        find_enclosing_key(&state, 0x1200, 0x1600),
        Some((0x1000, 0x1000))
    );
    // Requests not enclosed by (0x1000, 0x1000) still resolve to the unpinned super-span.
    assert_eq!(
        find_enclosing_key(&state, 0x1000, 0x3000),
        Some((0x1000, 0x2000))
    );
    assert!(has_overlapping_registered_span(&state, 0x1800, 0x2400));
    assert!(!has_overlapping_registered_span(&state, 0x2000, 0x3000));
}

#[test]
fn pinned_span_guard_device_ptr_for_checks_bounds_and_offsets() {
    let cache = HostRegionPinCache::new();
    let guard = PinnedSpanGuard {
        inner: Arc::clone(&cache.inner),
        key: (0x10_0000, 0x4000),
        status: PinStatus::Pinned,
        dev_ptr_base: Some(0x7000_0000),
    };
    assert_eq!(guard.device_ptr_for(0x10_0000, 0x100), Some(0x7000_0000));
    assert_eq!(guard.device_ptr_for(0x10_1000, 0x3000), Some(0x7000_1000));
    assert_eq!(guard.device_ptr_for(0x0F_FFF0, 0x100), None);
    assert_eq!(guard.device_ptr_for(0x10_3000, 0x1001), None);
}
