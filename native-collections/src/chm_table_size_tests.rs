use super::chm_jdk_table_size_for;

/// Every one of these is a reading of `ConcurrentHashMap.table.length` on
/// HotSpot JDK 25, not a value derived from the constructor's source. The
/// source reads like `tableSizeFor(c + (c >>> 1) + 1)`, which would put
/// `CHM(11)` at 32; the real map holds 16.
#[test]
fn matches_the_measured_jdk_table_sizes() {
    // (initial capacity or size, table length) — sized constructor sweep.
    let measured = [
        (1, 2),
        (2, 4),
        (3, 8),
        (4, 8),
        (5, 8),
        (6, 16),
        (11, 16),
        (12, 32),
        (23, 32),
        (24, 64),
        (47, 64),
        (48, 128),
        (95, 128),
        (512, 1024),
    ];
    for (n, want) in measured {
        assert_eq!(
            chm_jdk_table_size_for(n),
            want,
            "table size for {n} entries"
        );
    }
}

/// A default map's growth points, read the same way: it doubles as the
/// size reaches 12, 24, 48, 96, 192.
#[test]
fn matches_the_measured_default_map_growth() {
    for (size, want) in [
        (1, 16),
        (11, 16),
        (12, 32),
        (23, 32),
        (24, 64),
        (47, 64),
        (48, 128),
        (95, 128),
        (96, 256),
        (191, 256),
        (192, 512),
    ] {
        assert_eq!(
            chm_jdk_table_size_for(size).max(16),
            want,
            "default map holding {size} entries"
        );
    }
}

/// The threshold is STRICT: a table of size P holds up to `0.75P - 1`
/// entries, and the entry that reaches `0.75P` is what doubles it. An
/// off-by-one here shifts every key's bucket for a whole size band.
#[test]
fn the_load_threshold_is_strict() {
    for bits in 4..20 {
        let cap = 1usize << bits;
        let threshold = cap - (cap >> 2);
        assert_eq!(chm_jdk_table_size_for(threshold - 1), cap);
        assert_eq!(chm_jdk_table_size_for(threshold), cap << 1);
    }
}
