use super::*;

#[test]
fn zr10mt_the_mode_switch_reads_off_all_and_defaults() {
    assert_eq!(parse_mode(None, SparseMode::Zeroed), SparseMode::Zeroed);
    assert_eq!(parse_mode(None, SparseMode::Off), SparseMode::Off);
    assert_eq!(
        parse_mode(Some(b"  "), SparseMode::Zeroed),
        SparseMode::Zeroed
    );
    for v in [&b"0"[..], b"off", b" FALSE ", b"no"] {
        assert_eq!(
            parse_mode(Some(v), SparseMode::Zeroed),
            SparseMode::Off,
            "{v:?}"
        );
    }
    for v in [&b"1"[..], b"on", b"zeroed", b"yes"] {
        assert_eq!(
            parse_mode(Some(v), SparseMode::Off),
            SparseMode::Zeroed,
            "{v:?}"
        );
    }
    assert_eq!(parse_mode(Some(b"ALL"), SparseMode::Off), SparseMode::All);
}

#[test]
fn zr10mt_the_threshold_parses_decimal_kib_with_a_floor() {
    assert_eq!(parse_min_kb(None), DEFAULT_MIN_KB);
    assert_eq!(parse_min_kb(Some(b"")), DEFAULT_MIN_KB);
    assert_eq!(parse_min_kb(Some(b"2048")), 2048);
    assert_eq!(parse_min_kb(Some(b" 512 ")), 512);
    assert_eq!(
        parse_min_kb(Some(b"4")),
        FLOOR_MIN_KB,
        "clamped up to the floor"
    );
    assert_eq!(
        parse_min_kb(Some(b"1m")),
        DEFAULT_MIN_KB,
        "no suffixes: unparsable"
    );
    assert_eq!(parse_min_kb(Some(b"-1")), DEFAULT_MIN_KB);
    assert_eq!(
        parse_min_kb(Some(b"99999999999999999999999999")),
        DEFAULT_MIN_KB
    );
}

#[test]
fn zr10mt_only_big_page_aligned_requests_of_the_mode_are_routed() {
    let min = 1 << 20;
    assert!(routes(SparseMode::Zeroed, true, min, 8, min));
    assert!(
        !routes(SparseMode::Zeroed, false, min, 8, min),
        "zeroed mode leaves plain alloc"
    );
    assert!(
        !routes(SparseMode::Zeroed, true, min - 1, 8, min),
        "under the threshold"
    );
    assert!(
        !routes(SparseMode::Zeroed, true, min, 2 * PAGE, min),
        "over-page alignment"
    );
    assert!(routes(SparseMode::All, false, min, PAGE, min));
    assert!(!routes(SparseMode::Off, true, 1 << 30, 8, min));
    assert!(!routes(SparseMode::Undecided, true, 1 << 30, 8, min));
}

#[test]
fn zr11rv9_only_a_v3_backend_may_map() {
    assert!(backend_supports_mapping(332));
    assert!(backend_supports_mapping(300));
    assert!(!backend_supports_mapping(219));
    assert!(!backend_supports_mapping(0));
}

#[test]
fn zr10mt_map_len_is_whole_pages() {
    assert_eq!(map_len(1), PAGE);
    assert_eq!(map_len(PAGE), PAGE);
    assert_eq!(map_len(PAGE + 1), 2 * PAGE);
    assert_eq!(map_len(1 << 20), 1 << 20);
}

#[test]
fn zs9rs2_boot_mode_parses_routes_zeroed_and_counts_its_budget() {
    assert_eq!(parse_mode(Some(b"boot"), SparseMode::Off), SparseMode::Boot);
    assert_eq!(
        parse_mode(Some(b" BOOT "), SparseMode::Off),
        SparseMode::Boot
    );
    assert_eq!(
        SparseMode::from_u8(SparseMode::Boot as u8),
        SparseMode::Boot
    );
    assert_eq!(SparseMode::Boot.name(), "boot");
    let min = 1 << 20;
    assert!(routes(SparseMode::Boot, true, min, 8, min));
    assert!(
        !routes(SparseMode::Boot, false, min, 8, min),
        "only zeroed blocks"
    );
    assert_eq!(parse_boot_count(None), DEFAULT_BOOT_COUNT);
    assert_eq!(parse_boot_count(Some(b"")), DEFAULT_BOOT_COUNT);
    assert_eq!(parse_boot_count(Some(b"x1")), DEFAULT_BOOT_COUNT);
    assert_eq!(parse_boot_count(Some(b" 0 ")), 0);
    assert_eq!(parse_boot_count(Some(b"128")), 128);
}

/// zt t2/fp: a refund puts back exactly the mapping a take removed. Skipped
/// when this test binary's own allocator latched mode `boot` (it would
/// race the budget).
#[test]
fn zt2fp_a_refused_boot_mapping_refunds_its_budget() {
    if SparseMode::from_u8(MODE.load(Ordering::Acquire)) == SparseMode::Boot {
        return;
    }
    let before = BOOT_BUDGET.load(Ordering::Acquire);
    refund_boot_budget();
    assert_eq!(BOOT_BUDGET.load(Ordering::Acquire), before + 1);
    assert!(take_boot_budget(), "the refunded mapping can be taken");
    assert_eq!(BOOT_BUDGET.load(Ordering::Acquire), before);
}

#[test]
fn zs6rs_census_buckets_partition_the_sizes() {
    assert_eq!(census_bucket(0), 0);
    assert_eq!(census_bucket(64), 0);
    assert_eq!(census_bucket(65), 1);
    assert_eq!(census_bucket(512), 1);
    assert_eq!(census_bucket(8 << 10), 2);
    assert_eq!(census_bucket((8 << 10) + 1), 3);
    assert_eq!(census_bucket(64 << 10), 3);
    assert_eq!(census_bucket((1 << 20) - 1), 4);
    assert_eq!(census_bucket(1 << 20), 5);
    assert_eq!(census_bucket(usize::MAX), 5);
}

#[test]
fn zs6rs_census_line_is_off_unless_latched() {
    // The test binary's allocator latched from its own environment; the
    // line exists exactly when the census is on, and then names every
    // bucket.
    match census_line() {
        None => assert!(!CENSUS.load(Ordering::Relaxed)),
        Some(line) => {
            for (name, _) in CENSUS_BUCKETS {
                assert!(line.contains(&format!(" live_{name}_kb=")), "{line}");
                assert!(line.contains(&format!(" peak_{name}_kb=")), "{line}");
            }
        }
    }
}

#[test]
fn zr10mt_the_summary_line_names_every_counter() {
    let line = summary_line();
    for key in [
        "[cratonvm] mimalloc-sparse-blocks: mode=",
        " min_kb=",
        " big_allocs=",
        " big_zeroed_allocs=",
        " big_zeroed_kb=",
        " big_live_kb=",
        " big_live_peak_kb=",
        " mapped=",
        " mapped_live_kb=",
        " mapped_peak_kb=",
        " remaps=",
        " map_failed=",
        " boot_budget_left=",
        " boot_declined=",
    ] {
        assert!(line.contains(key), "{key} missing from {line}");
    }
}
