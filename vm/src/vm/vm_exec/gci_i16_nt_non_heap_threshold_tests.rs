use super::non_heap_pool_as_jmx_usage;
use cratonvm_gc::gc_metrics::{GcNotificationQueue, JmxPoolUsage, PoolThresholdKind};

fn samples(metadata: u64, code: u64) -> Vec<JmxPoolUsage> {
    cratonvm_native_builtins_jmx::jmx::non_heap_threshold_samples(
        Some((metadata, None)),
        Some((code, Some(240 << 20))),
    )
    .iter()
    .map(non_heap_pool_as_jmx_usage)
    .collect()
}

/// The threshold table's sample of a non-heap pool keeps `getUsage()`'s
/// quadruple: `init` 0 stays defined, `max` -1 is undefined.
#[test]
fn gci_i16_nt_a_non_heap_sample_keeps_the_get_usage_figures() {
    let s = samples(5_000, 300_000);
    let meta = s.iter().find(|p| p.name == "Metaspace").expect("sampled");
    assert_eq!(
        (meta.init, meta.used, meta.committed, meta.max, meta.collection_used),
        (Some(0), 5_000, 5_000, None, None)
    );
    assert_eq!(meta.managers, ["Metaspace Manager"]);
    let code = s
        .iter()
        .find(|p| p.name == "CodeHeap 'non-profiled nmethods'")
        .expect("sampled");
    assert_eq!(
        (code.used, code.committed, code.max),
        (300_000, 300_000, Some(240 << 20))
    );
    assert_eq!(s.len(), 5);
}

/// End to end over the table the VM arms: a `Metaspace` threshold crossed
/// by class growth triggers once with the usage `getUsage()` would
/// answer, and a code-heap threshold is independent of it.
#[test]
fn gci_i16_nt_metaspace_and_code_heap_thresholds_trigger_on_crossing() {
    let q = GcNotificationQueue::new();
    let t = q.pool_thresholds();
    t.set("Metaspace", PoolThresholdKind::Usage, 6_000);
    t.set("CodeHeap 'non-profiled nmethods'", PoolThresholdKind::Usage, 1 << 20);
    t.check(&samples(5_000, 300_000), &[]);
    assert!(!q.has_pending(), "both below");
    t.check(&samples(6_500, 300_000), &[]);
    let reqs = t.take_requests();
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    assert_eq!(reqs[0].pool, "Metaspace");
    assert_eq!(reqs[0].trigger_usage, Some([0, 6_500, 6_500, -1]));
    t.check(&samples(9_000, 2 << 20), &[]);
    let reqs = t.take_requests();
    assert_eq!(reqs.len(), 1, "Metaspace stays on; the code heap crosses: {reqs:?}");
    assert_eq!(reqs[0].pool, "CodeHeap 'non-profiled nmethods'");
    assert_eq!(reqs[0].count, 1);
}
