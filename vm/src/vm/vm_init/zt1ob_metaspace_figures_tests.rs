use super::{class_metadata_walk_bytes, class_metadata_walk_figures, metaspace_stats, SharedVm};
use super::{class_metadata_prefix_bytes, class_metadata_prefix_figures};
use crate::config::VmConfig;

/// zt t3/ob: a back-fill prefix past every class states the whole walk's
/// figures (committed and class space included), and its `used` is the
/// used-only prefix walk's; the empty prefix states nothing used.
#[test]
fn zt3ob_a_prefix_past_every_class_states_the_whole_walk() {
    let shared = SharedVm::new(VmConfig::default());
    let cm = shared.classes.class_manager.read();
    let end = cm.class_store().next_id().as_u32();
    let got = class_metadata_prefix_figures(&cm, &[0, end]);
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].1.used, 0);
    assert_eq!(got[1], (end, class_metadata_walk_figures(&cm)));
    assert_eq!(got[1].1.used, class_metadata_prefix_bytes(&cm, &[end])[0].1);
}

/// zt t1/ob: the figures walk states the used walk's figure and
/// `metaspace_stats`' committed and class space (one model, one walk).
#[test]
fn zt1ob_the_figures_walk_states_the_used_walks_figure() {
    let shared = SharedVm::new(VmConfig::default());
    let cm = shared.classes.class_manager.read();
    let f = class_metadata_walk_figures(&cm);
    assert_eq!(f.used, class_metadata_walk_bytes(&cm) as u64);
    let stats = metaspace_stats(&cm);
    assert_eq!(
        (f.used, f.committed, f.class_space),
        (stats.used, stats.committed, stats.class_space)
    );
}

/// zt t1/ob: the GC crate's copy of HotSpot's non-heap pool names (a ZGC
/// pause record's zero keys) is the JMX list the pool beans are built
/// from, in the same order, each with one of the two plain managers.
#[test]
fn zt1ob_the_gc_crates_non_heap_pool_names_are_the_jmx_list() {
    let pools = cratonvm_gc::gc_metrics::HOTSPOT_NON_HEAP_POOLS;
    let names: Vec<&str> = pools.iter().map(|&(n, _)| n).collect();
    assert_eq!(names, cratonvm_native_api::registry::JMX_NON_HEAP_POOL_NAMES);
    for (name, managers) in pools {
        assert_eq!(managers.len(), 1, "{name}");
        assert!(
            cratonvm_native_api::registry::JMX_NON_HEAP_MANAGER_NAMES.contains(&managers[0]),
            "{name}"
        );
    }
    // zt t30/nh: and the unsegmented code cache's list.
    let names: Vec<&str> = cratonvm_gc::gc_metrics::HOTSPOT_UNSEGMENTED_NON_HEAP_POOLS
        .iter()
        .map(|&(n, _)| n)
        .collect();
    assert_eq!(
        names,
        cratonvm_native_api::registry::JMX_UNSEGMENTED_NON_HEAP_POOL_NAMES
    );
}
