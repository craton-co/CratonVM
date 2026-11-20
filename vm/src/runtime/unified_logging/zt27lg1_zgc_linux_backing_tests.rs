use super::*;

const M: u64 = 1024 * 1024;

#[test]
fn zt27lg1_backing_lines_and_map_count_warning_are_hotspots() {
    let unlimited = ZgcLinuxBacking {
        address_space_limit: None,
        max_map_count: Some(1_048_576),
    };
    // `tg10_h3` (ergonomic 1986M): 16 x 1986M = 31776M, not a whole G.
    assert_eq!(
        zgc_linux_backing_lines(1986 * M, &unlimited, false),
        [
            "Address Space Size: unlimited",
            "Reserved Space Type: Contiguous/Unrestricted/Complete",
            "Reserved Space Size: 31776M",
            "Heap Backing File: /memfd:java_heap",
            "Heap Backing Filesystem: tmpfs (0x1021994)",
        ]
    );
    // `h7_T7` (`-Xmx256m`): 16 x 256M = 4G.
    assert_eq!(
        zgc_linux_backing_lines(256 * M, &unlimited, false)[2],
        "Reserved Space Size: 4G"
    );
    // A limit that binds: 8G / 2 = 4G, below 16 x 1G and below the 4T
    // offset maximum (16G rounds to 2^34, clamped up to 2^42).
    let limited = ZgcLinuxBacking {
        address_space_limit: Some(8 * 1024 * M),
        max_map_count: None,
    };
    let lines = zgc_linux_backing_lines(1024 * M, &limited, false);
    let huge = zgc_linux_backing_lines(1024 * M, &limited, true);
    assert_eq!(huge[3], "Heap Backing File: /memfd:java_heap.hugetlb");
    assert_eq!(huge[4], "Heap Backing Filesystem: hugetlbfs (0x958458f6)");
    assert_eq!(lines[0], "Address Space Size: limited (8G)");
    assert_eq!(lines[1], "Reserved Space Type: Contiguous/Restricted/Complete");
    assert_eq!(lines[2], "Reserved Space Size: 4G");
    // 16 x 1986M rounds up to 32G = 2^35 -> clamped to 2^42; 16 x 1T =
    // 2^44; 16 x 20T rounds to 2^49 -> clamped to 2^44 (16384G).
    assert_eq!(zgc_address_offset_max(1986 * M), 1 << 42);
    assert_eq!(zgc_address_offset_max(1 << 40), 1 << 44);
    assert_eq!(zgc_address_offset_max(20 << 40), 1 << 44);
    assert_eq!(ZGC_MAX_SUPPORTED_HEAP_BYTES / (1024 * M), 16384);
    // `tg8_s2` (`-Xmx1t`, limit 1048576): 524288 granules x 3 x 1.2 =
    // 1887436.8, truncated to 1887436.
    let w = zgc_max_map_count_warning_lines(1 << 40, Some(1_048_576));
    assert_eq!(w.len(), 5);
    assert_eq!(
        w[0],
        "***** WARNING! INCORRECT SYSTEM CONFIGURATION DETECTED! *****"
    );
    assert_eq!(
        w[2],
        "max Java heap size (1048576M). Please adjust /proc/sys/vm/max_map_count to allow for at"
    );
    assert_eq!(
        w[3],
        "least 1887436 mappings (current limit is 1048576). Continuing execution with the current"
    );
    // Enough mappings (exactly the requirement), an unreadable limit, or
    // a small heap (993 granules x 3.6 = 3574 < 65530): nothing.
    assert!(zgc_max_map_count_warning_lines(1 << 40, Some(1_887_436)).is_empty());
    assert!(zgc_max_map_count_warning_lines(1 << 40, None).is_empty());
    assert!(zgc_max_map_count_warning_lines(1986 * M, Some(65_530)).is_empty());
    assert_eq!(
        gc_warning_default_line(0.0081, 2, "x"),
        "[0.008s][warning][gc] x"
    );
    assert_eq!(
        gc_warning_default_line(0.0081, 8, "x"),
        "[0.008s][warning][gc      ] x"
    );
}

/// The five lines sit between `Large Page Support` and `Min Capacity`
/// (host order), only with the facts.
#[test]
fn zt27lg1_backing_lines_sit_after_the_large_page_line() {
    let m = 1024 * 1024usize;
    let mut f = GcInitFacts {
        collector: GC_NAME_ZGC,
        version: "25.0.4",
        cpus_total: 4,
        cpus_available: 4,
        memory_bytes: None,
        heap_min: 8 * m,
        heap_initial: 126 * m,
        heap_max: 256 * m,
        compressed_oops: "Disabled",
        region_size: None,
        soft_max: None,
        zgc_uncommit: Some((true, 300)),
        zgc_runtime_workers: None,
        zgc_uncommit_unsupported: false,
        pre_touch: false,
        large_pages: None,
        g1_parallel_workers: None,
        g1_periodic_interval_ms: 0,
        zgc_linux_backing: Some(ZgcLinuxBacking {
            address_space_limit: None,
            max_map_count: Some(65_530),
        }),
    };
    let on = zgc_init_lines(&f);
    let at = on
        .iter()
        .position(|l| l.starts_with("Large Page Support: "))
        .expect("the large page line");
    assert_eq!(on[at + 1], "Address Space Size: unlimited", "{on:?}");
    assert_eq!(on[at + 5], "Heap Backing Filesystem: tmpfs (0x1021994)");
    assert!(on[at + 6].starts_with("Min Capacity: "), "{on:?}");
    f.zgc_linux_backing = None;
    let none = zgc_init_lines(&f);
    assert!(!none.iter().any(|l| l.starts_with("Reserved Space")));
    assert!(none[at + 1].starts_with("Min Capacity: "), "{none:?}");
}
