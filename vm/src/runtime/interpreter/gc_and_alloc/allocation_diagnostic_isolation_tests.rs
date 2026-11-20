use super::*;
use std::sync::atomic::Ordering;
#[test]
fn frame_demand_and_allocation_counters_are_owned_by_one_vm() {
    let a = SharedVm::new(crate::config::VmConfig::default());
    let b = SharedVm::new(crate::config::VmConfig::default());
    let demand = FrameTraceWanted::raise(&a.mem.diagnostics.frame_trace_wanted);
    assert!(frame_trace_wanted(&a));
    assert!(!frame_trace_wanted(&b));
    a.mem
        .diagnostics
        .rootsnap_calls
        .store(11, Ordering::Relaxed);
    a.mem
        .diagnostics
        .tlab_compact_objects
        .store(3, Ordering::Relaxed);
    assert_eq!(b.mem.diagnostics.rootsnap_calls.load(Ordering::Relaxed), 0);
    assert_eq!(tlab_object_shape_counts(&b), (0, 0, 0));
    assert_eq!(tlab_object_shape_counts(&a).0, 3);
    drop(demand);
    assert!(!frame_trace_wanted(&a));
    assert!(!frame_trace_wanted(&b));
}
