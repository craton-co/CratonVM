use super::*;
use crate::memory::vm_heap::VmHeap;

#[test]
fn helper_selection_uses_the_owning_zgc_gate_block() {
    let a = VmHeap::new(cratonvm_gc::GcBackend::Zgc, 8 * 1024 * 1024);
    let b = VmHeap::new(cratonvm_gc::GcBackend::Zgc, 8 * 1024 * 1024);
    let pa = ref_store_plan_for_heap(Some(&a));
    let pb = ref_store_plan_for_heap(Some(&b));
    assert_ne!(pa.0 .0, pb.0 .0);
    assert_ne!(pa.0 .1, pb.0 .1);
    if a.is_zgc() {
        let own = a.zgc_jit_ref_store_plan().unwrap();
        assert_eq!(pa, ((own.0, own.1, own.2), 0));
    }
    if b.is_zgc() {
        b.zgc_set_mark_active(false);
        b.zgc_set_generational_enabled(false);
    }
    drop(b);
    assert_eq!(ref_store_plan_for_heap(Some(&a)), pa);
}
