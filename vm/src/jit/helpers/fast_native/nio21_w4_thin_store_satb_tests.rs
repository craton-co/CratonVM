use super::*;
use crate::config::VmConfig;
use cratonvm_gc::{ConcurrentGcState, SatbQueue};
use std::sync::Arc;

// nio21 wave 5 L1: the pure decision test of the wave-4 copy
// (`thin_store_must_publish`) went with the copy; the decision is
// `vm_exec::publish_overwritten_field`'s, tested by
// `r16w9_native_store_satb_tests` against a mock heap.

/// The bug the page records: a thin `VarHandle` reference write on a
/// marking Generational heap must log the value it overwrites, as the
/// native's `NativeContextImpl::set_field*` does.
#[test]
fn nio21_w4_thin_store_publishes_the_overwritten_reference_while_generational_marks() {
    // The overwritten value is YOUNG: pin the old-only SATB filter off so
    // the barrier alone decides (as `jit_putfield_object_satb_pre_barrier_enqueues_old_ref`).
    let _flags = cratonvm_types::flags::override_thread(
        cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
            "CRATONVM_GEN_SATB_OLD_ONLY",
            Some("0"),
        )]),
    );
    let cfg = VmConfig {
        gc_algorithm: crate::config::GcAlgorithm::Generational,
        ..VmConfig::default()
    };
    let mut vm_box: Box<SharedVm> = Box::new(SharedVm::new(cfg));
    let satb: Arc<SatbQueue> = Arc::new(SatbQueue::new());
    let state: Arc<ConcurrentGcState> = Arc::new(ConcurrentGcState::new());
    vm_box
        .mem
        .heap
        .enable_concurrent_gc(satb.clone(), state.clone());
    let heap = &vm_box.mem.heap;
    let container = heap.alloc_object(ClassId::new(0), 1);
    let old_obj = heap.alloc_object(ClassId::new(0), 0);
    heap.set_field(container, 0, Value::Object(Some(old_obj)));

    // Nothing marks: nothing is read or published.
    assert!(!thin_store_publish_overwritten_field(
        heap, container, 0, b'L'
    ));

    satb.activate();
    state.set_phase(cratonvm_gc::ConcurrentGcPhase::ConcurrentMark);
    heap.flush_thread_satb();
    let _ = satb.drain();

    // A primitive descriptor never publishes; a reference one does.
    assert!(!thin_store_publish_overwritten_field(
        heap, container, 0, b'I'
    ));
    assert!(thin_store_publish_overwritten_field(
        heap, container, 0, b'L'
    ));
    heap.flush_thread_satb();
    let drained = satb.drain();
    assert!(
        drained.contains(&(old_obj.as_ptr() as usize)),
        "the thin store's pre-barrier must log the OLD reference; drained={drained:?}"
    );

    // A null old value publishes nothing.
    heap.set_field(container, 0, Value::Object(None));
    assert!(!thin_store_publish_overwritten_field(
        heap, container, 0, b'L'
    ));

    satb.deactivate_and_discard();
    state.set_phase(cratonvm_gc::ConcurrentGcPhase::Idle);
}
