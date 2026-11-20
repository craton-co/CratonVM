use super::*;

/// gci i14/hd item 9: G1's debug blocks -- the Serial-measured headers
/// over `print_heap_on`'s two lines (`gc,heap`) and the Metaspace line
/// (`gc,metaspace`), each prefixed `GC(n) `.
#[test]
fn gci_i14hd_g1_heap_debug_blocks_are_hotspots() {
    const K: u64 = 1024;
    let h = crate::runtime::unified_logging::G1ExitHeap {
        reserved: 131072 * K,
        committed: 8192 * K,
        used: 3 * 1024 * K,
        bounds: [0x7f00_0000_0000, 0x7f00_0800_0000],
        region_size: 1024 * K,
        young_regions: 3,
        survivor_regions: 0,
        metaspace: (94 * K, 94 * K, 94 * K),
        // gci i33/ms: the `class space` line goes with the Metaspace one.
        class_space: Some((5 * K, 64 * K, 1_048_576 * K)),
    };
    let (heap, meta) = render_xlog_g1_heap_debug_blocks(4, "Before", (4, 1), &h);
    assert_eq!(
        heap,
        vec![
            "GC(4) Heap Before GC invocations=4 (full 1):",
            "GC(4)  garbage-first heap   total reserved 131072K, committed 8192K, used 3072K [0x00007f0000000000, 0x00007f0008000000)",
            "GC(4)   region size 1024K, 3 young (3072K), 0 survivors (0K)",
        ]
    );
    assert_eq!(
        meta,
        vec![
            "GC(4) Metaspace Before GC invocations=4 (full 1):",
            "GC(4)  Metaspace       used 94K, committed 94K, reserved 94K",
            "GC(4)   class space    used 5K, committed 64K, reserved 1048576K",
        ]
    );
}

/// gci i14/hd item 9: no G1 figures off G1, nor without the debug tags.
#[test]
fn gci_i14hd_g1_heap_debug_sample_only_on_g1_with_the_tags() {
    let serial = crate::vm::SharedVm::new(crate::config::VmConfig {
        gc_algorithm: crate::config::GcAlgorithm::Generational,
        max_heap_size: 64 * 1024 * 1024,
        ..Default::default()
    });
    assert!(g1_heap_debug_sample(&serial, true).is_none());
    let g1 = crate::vm::SharedVm::new(crate::config::VmConfig {
        gc_algorithm: crate::config::GcAlgorithm::G1,
        max_heap_size: 64 * 1024 * 1024,
        ..Default::default()
    });
    assert!(g1_heap_debug_sample(&g1, false).is_none(), "no events, no walk");
    assert_eq!(g1_heap_debug_sample(&g1, true).is_some(), xlog_heap_debug_on());
}
