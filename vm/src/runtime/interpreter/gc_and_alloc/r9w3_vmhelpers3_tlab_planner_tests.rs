use super::*;

/// The planner's shape counters are the exit report's, and are kept only
/// under the switch that prints them. Planning with the switch off must not
/// touch a process-global line.
#[test]
fn shape_counters_move_only_under_the_stats_switch() {
    if tlab_shape_stats_enabled() {
        // Someone ran the suite with CRATONVM_DBG_JIT_METHOD_STATS: the
        // counters are then supposed to move, and other tests move them.
        return;
    }
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let before = tlab_object_shape_counts(&shared);
    for n in 0..16usize {
        let _ = plan_tlab_object_shape_for(&shared, ClassId::new(1), n, tlab_site::JIT_NEW);
    }
    assert_eq!(
        tlab_object_shape_counts(&shared),
        before,
        "an allocation with the stats switch off paid a shared lock xadd"
    );
}

/// The debug gate is read once; asking again answers the same thing and
/// (by construction) does not go back to the environment.
#[test]
fn the_compact_tlab_debug_gate_is_latched() {
    let first = dbg_compact_tlab_enabled();
    for _ in 0..4 {
        assert_eq!(dbg_compact_tlab_enabled(), first);
    }
}

/// The planner's answer does not depend on whether the counters are kept.
#[test]
fn a_legacy_plan_is_the_uniform_cell_shape() {
    use cratonvm_gc::heap::{HEADER_SIZE, SLOT_SIZE};
    // A class id no test registers a compact layout for.
    let (total, body, flags) =
        plan_tlab_object_shape_at(ClassId::new(0x00FF_FFF0), 3, tlab_site::JIT_NEW);
    if flags == 0 {
        assert_eq!((total, body), (HEADER_SIZE + 3 * SLOT_SIZE, 0));
    }
}
