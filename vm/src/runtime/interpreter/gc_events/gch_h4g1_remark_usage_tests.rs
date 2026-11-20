use super::*;
use cratonvm_gc::gc_metrics::{
    gc_notifications_between, GcBeanSnapshot, JmxCollector, JmxPoolUsage,
};

const OLD: &[&str] = &["G1 Old Gen"];
const ALL: &[&str] = &["G1 Eden Space", "G1 Survivor Space", "G1 Old Gen"];

fn pool(name: &'static str, used: u64, collection_used: Option<u64>) -> JmxPoolUsage {
    JmxPoolUsage {
        name,
        init: None,
        used,
        committed: used,
        max: None,
        collection_used,
        managers: &[],
    }
}

fn collector(name: &'static str, count: u64, pools: &'static [&'static str]) -> JmxCollector {
    JmxCollector {
        name,
        count,
        time_ms: 0,
        time_ns: 0,
        pools,
        action: "end of concurrent GC pause",
    }
}

/// A remark + cleanup moves `G1 Concurrent GC` only: `G1 Old Gen` takes
/// the pause's usage as its collection usage, eden and survivor keep their
/// last young pause's. Before any collection the others read 0 (gci
/// i5g/g1o; was: no stamp).
#[test]
fn gch_h4g1_a_remark_stamps_only_the_old_pools_collection_usage() {
    let before = GcBeanSnapshot::now(
        vec![
            collector("G1 Young Generation", 3, ALL),
            collector("G1 Concurrent GC", 1, OLD),
        ],
        vec![
            pool("G1 Eden Space", 0, Some(0)),
            pool("G1 Survivor Space", 256, Some(256)),
            pool("G1 Old Gen", 9500, Some(7000)),
        ],
    );
    let after = GcBeanSnapshot::now(
        vec![
            collector("G1 Young Generation", 3, ALL),
            collector("G1 Concurrent GC", 2, OLD),
        ],
        vec![
            pool("G1 Eden Space", 4096, Some(0)),
            pool("G1 Survivor Space", 256, Some(256)),
            pool("G1 Old Gen", 9000, Some(7000)),
        ],
    );
    let recs = gc_notifications_between(&before, &after, "No GC", 0);
    assert_eq!(recs.len(), 1, "only the concurrent bean moved");
    let managed = g1_remark_managed_pools(&after, &recs);
    assert_eq!(managed, vec!["G1 Old Gen"]);
    let rows = affected_collection_usage_rows(&after.pools, &managed);
    let used: Vec<(&str, u64)> = rows.iter().map(|p| (p.name, p.used)).collect();
    assert_eq!(
        used,
        vec![
            ("G1 Eden Space", 0),
            ("G1 Survivor Space", 256),
            ("G1 Old Gen", 9000)
        ]
    );
    // gci i5g/g1o: before any collection the remark still stamps the old
    // pool; the others read 0, as an unstamped pool reports.
    let fresh = [pool("G1 Eden Space", 1, None), pool("G1 Old Gen", 2, None)];
    let rows = affected_collection_usage_rows(&fresh, &managed);
    let used: Vec<u64> = rows.iter().map(|p| p.used).collect();
    assert_eq!(used, vec![0, 2]);
    // A pause that moved no bean manages nothing.
    assert!(g1_remark_managed_pools(&after, &[]).is_empty());
}

/// gci i5g/g1o (`gch-h4g1-g1-young-only-pauses-stamp-the-old-pools-collection-usage`):
/// a young-only G1 pause stamps eden and survivor and writes `G1 Old Gen`
/// back with its own last figure (0 before any); a mixed pause (the count
/// moved) affects every pool.
#[test]
fn gci_i5g_g1o_a_young_only_pause_leaves_the_old_pools_collection_usage() {
    assert!(g1_young_only_pause(4, 4), "no Mixed record: young-only");
    assert!(!g1_young_only_pause(4, 5), "a Mixed record: all pools");
    assert_eq!(
        G1_YOUNG_ONLY_AFFECTED_POOLS,
        &["G1 Eden Space", "G1 Survivor Space"][..]
    );
    let after = [
        pool("G1 Eden Space", 0, Some(0)),
        pool("G1 Survivor Space", 512, Some(256)),
        pool("G1 Old Gen", 9000, Some(7000)),
    ];
    let rows = affected_collection_usage_rows(&after, G1_YOUNG_ONLY_AFFECTED_POOLS);
    let used: Vec<(&str, u64)> = rows.iter().map(|p| (p.name, p.used)).collect();
    assert_eq!(
        used,
        vec![
            ("G1 Eden Space", 0),
            ("G1 Survivor Space", 512),
            ("G1 Old Gen", 7000)
        ]
    );
    // Before any stamp the old pool keeps HotSpot's initial 0.
    let fresh = [
        pool("G1 Eden Space", 0, None),
        pool("G1 Survivor Space", 128, None),
        pool("G1 Old Gen", 4096, None),
    ];
    let rows = affected_collection_usage_rows(&fresh, G1_YOUNG_ONLY_AFFECTED_POOLS);
    let used: Vec<u64> = rows.iter().map(|p| p.used).collect();
    assert_eq!(used, vec![0, 128, 0]);
    // Every pool affected: the pools as they are.
    assert_eq!(affected_collection_usage_rows(&after, ALL), after.to_vec());
}

/// gci i5g/g1o (the G1 half of
/// `gengc-r4-plumbing-jfr-phase-and-concurrent-events-not-emitted`): G1's
/// marking pauses get `jdk.GCPhasePause` rows, the initial mark opens the
/// cycle, and each kind finds its cycle state on its own backend.
#[test]
fn gci_i5g_g1o_g1_marking_pauses_have_cycle_jfr_rows() {
    use super::NonCollectionPause as P;
    // gci i8/jf: G1's initial mark is HotSpot G1's `GC Pause`.
    assert_eq!(cycle_jfr_pause_name(P::G1InitialMark), Some("GC Pause"));
    assert_eq!(cycle_jfr_pause_name(P::G1Remark), Some("Pause Remark"));
    assert_eq!(cycle_jfr_pause_name(P::GenInitialMark), Some("Pause Init Mark"));
    assert_eq!(cycle_jfr_pause_name(P::GenRemark), Some("Pause Remark"));
    for other in [
        P::FrameTrace,
        P::HeapDump,
        P::HeapInspection,
        P::LoopExit,
        P::Redefinition,
        P::ZgcMarkStart,
        P::IcGrace,
    ] {
        assert_eq!(cycle_jfr_pause_name(other), None, "{other:?}");
    }
    assert!(cycle_jfr_pause_opens_the_cycle(P::G1InitialMark));
    assert!(cycle_jfr_pause_opens_the_cycle(P::GenInitialMark));
    assert!(!cycle_jfr_pause_opens_the_cycle(P::G1Remark));
    assert!(!cycle_jfr_pause_opens_the_cycle(P::GenRemark));
    assert!(cycle_jfr_pause_is_generational(P::GenRemark));
    assert!(!cycle_jfr_pause_is_generational(P::G1Remark));
    assert!(!cycle_jfr_pause_is_generational(P::G1InitialMark));
}

/// gci i8/jf (`gengc-r4-plumbing-jfr-phase-and-concurrent-events-not-emitted`,
/// G1 half): HotSpot G1's shape. The initial mark is `GC Pause` under its
/// own id and the cycle takes the next one; the concurrent marking is
/// `Concurrent Mark From Roots`; the remark that reached its cleanup is
/// `Pause Remark` then `Pause Cleanup`, partitioning the pause. The
/// Generational names and id count are unchanged.
#[test]
fn gci_i8_jf_g1_cycle_rows_take_hotspot_g1_names_and_ids() {
    use super::NonCollectionPause as P;
    assert_eq!(cycle_jfr_pause_name(P::GenInitialMark), Some("Pause Init Mark"));
    assert_ne!(cycle_jfr_pause_name(P::G1InitialMark), Some("Pause Init Mark"));
    assert_eq!(cycle_jfr_concurrent_mark_name(true), "Concurrent Mark");
    assert_eq!(cycle_jfr_concurrent_mark_name(false), "Concurrent Mark From Roots");
    // Ids (gci i12/ge): the cycle takes one; G1's initial mark writes no
    // row (its concurrent-start collection wrote the `GC Pause`), the
    // Generational initial mark keeps its `Pause Init Mark`.
    assert!(g1_initial_mark_has_no_row(P::G1InitialMark));
    assert!(!g1_initial_mark_has_no_row(P::GenInitialMark));
    assert!(!g1_initial_mark_has_no_row(P::G1Remark));
    // Remark rows: no cleanup point -> one row, as before.
    assert_eq!(
        remark_cleanup_rows(1_000, 900, None),
        vec![("Pause Remark", 1_000, 900)]
    );
    // A cleanup point -> Remark then Cleanup, partitioning the pause.
    let rows = remark_cleanup_rows(1_000, 900, Some(600));
    assert_eq!(
        rows,
        vec![("Pause Remark", 1_000, 600), ("Pause Cleanup", 1_600, 300)]
    );
    assert_eq!(rows.iter().map(|r| r.2).sum::<u64>(), 900);
    // A cut past the work is clamped: an empty cleanup row, never an
    // underflow.
    assert_eq!(
        remark_cleanup_rows(1_000, 900, Some(5_000)),
        vec![("Pause Remark", 1_000, 900), ("Pause Cleanup", 1_900, 0)]
    );
}
