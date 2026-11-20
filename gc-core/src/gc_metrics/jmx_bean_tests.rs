use super::*;

#[test]
fn a_full_collection_counts_under_the_full_collector_only() {
    // 10 collections, 3 of which also reclaimed old gen.
    // gen r5w3/obs7: the times are whole milliseconds of each bean's
    // MICROSECOND sum (HotSpot's accumulated timer), not the per-pause
    // ceiled `.2` figure (40 / 25 here), which the beans no longer read.
    let [young, old] = serial_collectors((10, 12_345, 40), (3, 9_000, 25));
    assert_eq!((young.name, young.count, young.time_ms), ("Copy", 7, 3));
    assert_eq!(
        (old.name, old.count, old.time_ms),
        ("MarkSweepCompact", 3, 9)
    );
    assert_eq!(
        young.count + old.count,
        10,
        "the beans must partition the collections, so a summing caller \
         (H2's collectGarbage) sees what the single bean used to report"
    );
    // Each bean floors its own sum, so the two times sum to within one
    // millisecond of the total's floor, never above it.
    let total_ms = 12_345 / 1_000;
    assert!(young.time_ms + old.time_ms <= total_ms);
    assert!(young.time_ms + old.time_ms + 1 >= total_ms);
}

/// gen r5w3/obs7 (`gengc-r5w2-obs6-collection-time-ceils-every-pause`):
/// 1 000 young pauses of 200 µs read 200 ms, as on HotSpot Serial — not
/// 1 000 (one ceiled millisecond per pause).
#[test]
fn collection_time_is_the_floor_of_the_microsecond_sum() {
    let [young, old] = serial_collectors((1_000, 200_000, 1_000), (0, 0, 0));
    assert_eq!((young.count, young.time_ms), (1_000, 200));
    assert_eq!((old.count, old.time_ms), (0, 0));
    // A single sub-millisecond pause does not move the time (HotSpot's
    // behaviour); the count does.
    let [young, _] = serial_collectors((1, 400, 1), (0, 0, 0));
    assert_eq!((young.count, young.time_ms), (1, 0));
}

#[test]
fn a_racing_old_snapshot_saturates_rather_than_wrapping() {
    // `old` read after a collection `all` did not see yet.
    let [young, old] = serial_collectors((4, 4_100, 4), (5, 5_200, 6));
    assert_eq!((young.count, young.time_ms), (0, 0));
    assert_eq!((old.count, old.time_ms), (4, 4));
}

#[test]
fn pool_and_collector_names_join_both_ways() {
    // JConsole joins `getMemoryPoolNames()` against the pool list; every
    // name a collector claims must be a pool, and every pool's managers
    // must be collectors that claim it. The pre-2026-09-23 answer failed
    // this (`Eden`/`Survivor` against `Eden Space`/`Old Gen`).
    let pools = [JMX_POOL_EDEN, JMX_POOL_SURVIVOR, JMX_POOL_TENURED];
    for c in serial_collectors((0, 0, 0), (0, 0, 0)) {
        for p in c.pools {
            assert!(pools.contains(p), "{} claims unknown pool {p}", c.name);
            assert!(serial_pool_managers(p).contains(&c.name));
        }
    }
    for p in pools {
        for m in serial_pool_managers(p) {
            let c = serial_collectors((0, 0, 0), (0, 0, 0))
                .into_iter()
                .find(|c| c.name == *m)
                .map(|c| c.pools)
                .unwrap_or(&[]);
            assert!(
                c.contains(&p),
                "{p} names manager {m}, which does not claim it"
            );
        }
    }
}

/// gci i14/mp (`gci-i14mp-memory-pool-and-gc-mxbean-review-findings-FIXED-20261016`
/// finding 1): a G1 heap's bean state keeps each pool's peak as its
/// collection edges and queries sample it (the VM feeds
/// `notifications().note_pool_peaks` from `gc_bean_snapshot` and from
/// `getPeakUsage()`), so the peak never falls when the pool shrinks, and
/// `resetPeakUsage()` lowers it to the usage at the reset.
#[test]
fn gci_i14_mp_a_backend_pool_peak_survives_the_pool_shrinking() {
    let beans = BackendGcBeans::new(BackendBeanShape::G1);
    let q = beans.notifications();
    let sample = |eden: u64, old: u64| {
        beans.pools(&[
            JmxPoolSample {
                init: Some(0),
                used: eden,
                committed: eden,
                max: None,
            },
            JmxPoolSample::default(),
            JmxPoolSample {
                init: Some(1 << 20),
                used: old,
                committed: 1 << 24,
                max: Some(1 << 26),
            },
        ])
    };
    assert_eq!(q.pool_peak(JMX_POOL_G1_EDEN), None, "nothing sampled yet");
    // The before edge of a pause: eden full, old holding 3 MiB.
    q.note_pool_peaks(&sample(8 << 20, 3 << 20));
    // The after edge and a later query: eden empty, old shrunk.
    q.note_pool_peaks(&sample(0, 1 << 20));
    assert_eq!(q.pool_peak(JMX_POOL_G1_EDEN), Some((8 << 20, 8 << 20)));
    assert_eq!(q.pool_peak(JMX_POOL_G1_OLD), Some((3 << 20, 1 << 24)));
    // `resetPeakUsage()` on the old pool: its usage now.
    q.reset_pool_peak(JMX_POOL_G1_OLD, 1 << 20, 1 << 24);
    assert_eq!(q.pool_peak(JMX_POOL_G1_OLD), Some((1 << 20, 1 << 24)));
    // Eden's peak is per pool: the old pool's reset left it alone.
    assert_eq!(q.pool_peak(JMX_POOL_G1_EDEN), Some((8 << 20, 8 << 20)));
    // Finding 13: one query's own sample, recorded by name.
    q.note_pool_peak(JMX_POOL_G1_EDEN, 9 << 20, 9 << 20);
    assert_eq!(q.pool_peak(JMX_POOL_G1_EDEN), Some((9 << 20, 9 << 20)));
}

/// gci i14/mp finding 3: a pool's collection committed is the one at the
/// end of the last collection that AFFECTED it; a young-only pause leaves
/// `G1 Old Gen` unstamped (HotSpot's `(init, 0, 0, max)`), and its
/// `collection_used` keeps the pre-i14 shape (0 once anything stamped).
#[test]
fn gci_i14_mp_collection_committed_follows_the_affecting_collection() {
    let beans = BackendGcBeans::new(BackendBeanShape::G1);
    let s = |used: u64, committed: u64| JmxPoolSample {
        init: Some(0),
        used,
        committed,
        max: None,
    };
    assert_eq!(beans.collection_committed(), [None, None, None]);
    // A young-only pause: eden and survivor only.
    let after = beans.pools(&[s(0, 4 << 20), s(1 << 20, 2 << 20), s(5 << 20, 32 << 20)]);
    beans.note_collection_usage_of(&after, Some(&[JMX_POOL_G1_EDEN, JMX_POOL_G1_SURVIVOR][..]));
    assert_eq!(beans.collection_committed(), [Some(4 << 20), Some(2 << 20), None]);
    let now = beans.pools(&[s(3 << 20, 8 << 20), s(1 << 20, 2 << 20), s(6 << 20, 40 << 20)]);
    assert_eq!(
        now.iter().map(|p| p.collection_used).collect::<Vec<_>>(),
        vec![Some(0), Some(1 << 20), Some(0)]
    );
    // A collection that affects every pool stamps the old pool too, with
    // ITS committed, which later growth does not move.
    beans.note_collection_usage(&beans.pools(&[s(0, 4 << 20), s(0, 0), s(7 << 20, 36 << 20)]));
    assert_eq!(beans.collection_committed(), [Some(4 << 20), Some(0), Some(36 << 20)]);
    let _ = beans.pools(&[s(0, 4 << 20), s(0, 0), s(9 << 20, 48 << 20)]);
    assert_eq!(beans.collection_committed()[2], Some(36 << 20));
}
