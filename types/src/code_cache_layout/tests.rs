use super::*;

fn tiered(cpus: u32) -> CodeCacheArgs {
    CodeCacheArgs {
        tiered: true,
        cpus,
        ..CodeCacheArgs::default()
    }
}

/// zki: the code cache's explicit huge pages on the host (25.0.4, 4 CPUs,
/// `-Xlog:pagesize*=debug`: `size: 242M` / `240M` / `48M`).
#[test]
fn zki_code_cache_large_page_bytes_match_the_host() {
    let lp = 2 * M;
    assert_eq!(
        hotspot_code_cache_large_page_bytes(&tiered(4), lp),
        Some(242 * M)
    );
    let unsegmented = CodeCacheArgs {
        segmented: Some(false),
        ..tiered(4)
    };
    assert_eq!(
        hotspot_code_cache_large_page_bytes(&unsegmented, lp),
        Some(240 * M)
    );
    let plain = CodeCacheArgs {
        cpus: 4,
        ..CodeCacheArgs::default()
    };
    assert_eq!(
        hotspot_code_cache_large_page_bytes(&plain, lp),
        Some(48 * M)
    );
    let small = CodeCacheArgs {
        reserved: Some(48 * M),
        ..tiered(4)
    };
    assert_eq!(
        hotspot_code_cache_large_page_bytes(&small, lp),
        Some(48 * M)
    );
    // Below eight large pages the code cache keeps small pages.
    let tiny = CodeCacheArgs {
        reserved: Some(12 * M),
        ..tiered(4)
    };
    assert_eq!(hotspot_code_cache_large_page_bytes(&tiny, lp), None);
    let sixteen = CodeCacheArgs {
        reserved: Some(16 * M),
        ..tiered(4)
    };
    assert_eq!(
        hotspot_code_cache_large_page_bytes(&sixteen, lp),
        Some(16 * M)
    );
}

/// Host, HotSpot 25.0.4, 4 CPUs (and `ActiveProcessorCount=1`): three
/// heaps of 2555904, sum 7667712; `ReservedCodeCacheSize=300m` the same.
#[test]
fn zt30nh_the_default_layout_is_three_heaps_of_2555904() {
    for cpus in [1, 4, 64] {
        let l = hotspot_code_cache_layout(&tiered(cpus));
        assert_eq!(
            l,
            CodeCacheLayout {
                reserved: l.reserved,
                ..CodeCacheLayout::DEFAULT_SEGMENTED
            }
        );
        assert_eq!(l.inits.iter().sum::<u64>(), 7_667_712);
    }
    let big = CodeCacheArgs {
        reserved: parse_hotspot_size("300m"),
        ..tiered(4)
    };
    assert_eq!(hotspot_code_cache_layout(&big).inits, [2_555_904; 3]);
}

/// Host: `ReservedCodeCacheSize=64m` (below 240M) or
/// `-XX:-SegmentedCodeCache`: one `CodeCache` pool of init 2555904.
#[test]
fn zt30nh_below_240m_or_unsegmented_is_one_code_cache() {
    let small = CodeCacheArgs {
        reserved: parse_hotspot_size("64m"),
        ..tiered(4)
    };
    let l = hotspot_code_cache_layout(&small);
    assert!(!l.segmented);
    assert_eq!(l.init_of(UNSEGMENTED_CODE_CACHE_POOL), Some(2_555_904));
    assert_eq!(l.reserved, 64 << 20);
    assert_eq!(l.init_of("CodeHeap 'non-nmethods'"), None);
    let off = CodeCacheArgs {
        segmented: Some(false),
        ..tiered(4)
    };
    let l = hotspot_code_cache_layout(&off);
    assert!(!l.segmented);
    assert_eq!(l.reserved, 240 << 20);
    let plain = CodeCacheArgs {
        tiered: false,
        ..tiered(4)
    };
    assert_eq!(hotspot_code_cache_layout(&plain).reserved, 48 << 20);
    assert!(!hotspot_code_cache_layout(&plain).segmented);
}

/// Host, 4 CPUs: `InitialCodeCacheSize=8m` -> non-nmethods 5836800 (its
/// whole size), profiled and non-profiled 8388608; sum 22614016.
#[cfg(not(windows))]
#[test]
fn zt30nh_initial_code_cache_size_moves_the_inits() {
    let a = CodeCacheArgs {
        initial: parse_hotspot_size("8m"),
        ..tiered(4)
    };
    let l = hotspot_code_cache_layout(&a);
    assert_eq!(l.inits, [5_836_800, 8_388_608, 8_388_608]);
    assert_eq!(l.inits.iter().sum::<u64>(), 22_614_016);
}

#[test]
fn zt30nh_compiler_counts_follow_cicompilercountpercpu() {
    assert_eq!(compiler_counts(1, None), (1, 1));
    assert_eq!(compiler_counts(2, None), (1, 1));
    assert_eq!(compiler_counts(4, None), (1, 2));
    assert_eq!(compiler_counts(8, None), (1, 3));
    assert_eq!(compiler_counts(16, None), (4, 8));
    assert_eq!(compiler_counts(4, Some(6)), (2, 4));
}

#[test]
fn zt30nh_hotspot_sizes_parse() {
    assert_eq!(parse_hotspot_size("64m"), Some(64 << 20));
    assert_eq!(parse_hotspot_size("2496K"), Some(2_555_904));
    assert_eq!(parse_hotspot_size("0x1000"), Some(4096));
    assert_eq!(parse_hotspot_size("1g"), Some(1 << 30));
    assert_eq!(parse_hotspot_size("123"), Some(123));
    assert_eq!(parse_hotspot_size("x"), None);
    assert_eq!(parse_hotspot_size(""), None);
}
