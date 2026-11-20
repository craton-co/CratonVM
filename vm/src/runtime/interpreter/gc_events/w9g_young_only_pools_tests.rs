use super::g1_young_only_pools;

#[test]
fn w9g_only_an_unmoved_g1_old_pause_count_is_young_only() {
    assert_eq!(
        g1_young_only_pools(Some(3), Some(3)),
        Some(cratonvm_gc::gc_metrics::G1_YOUNG_ONLY_POOLS)
    );
    assert_eq!(g1_young_only_pools(Some(3), Some(4)), None);
    assert_eq!(g1_young_only_pools(None, None), None);
}
