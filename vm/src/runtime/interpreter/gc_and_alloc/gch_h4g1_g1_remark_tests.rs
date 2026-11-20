use super::*;

#[test]
fn gch_h4g1_remark_keeps_reference_closure_and_itl_sweep_enabled() {
    assert!(g1_remark_unload_after_keep_enabled());
    assert!(g1_remark_itl_sweep_enabled());
}

/// Off G1 the keep-closure marker touches nothing (the generational remark
/// marks its own keep list inside `remark_finish`), and on G1 an empty
/// list returns before the heap is asked.
#[test]
fn gch_h4g1_the_keep_closure_marker_is_inert_off_g1_and_on_an_empty_list() {
    use crate::config::{GcAlgorithm, VmConfig};
    let generational = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    let obj = generational
        .mem
        .heap
        .alloc_object(cratonvm_types::ClassId::new(0), 1);
    g1_remark_mark_kept_closure(&generational, &[obj.as_ptr() as usize]);
    let g1 = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::G1,
        ..VmConfig::default()
    });
    g1_remark_mark_kept_closure(&g1, &[]);
}
