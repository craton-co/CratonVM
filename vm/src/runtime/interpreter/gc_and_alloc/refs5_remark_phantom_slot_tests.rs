use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use cratonvm_gc::ReferenceType;
use cratonvm_types::ClassId;

fn remark_nulls_enqueued_phantom_slots_on(gc_algorithm: GcAlgorithm) {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm,
        ..VmConfig::default()
    });
    let heap = &shared.mem.heap;
    let addr = |o: ObjectRef| o.as_ptr() as usize;
    // Reference-shaped: referent, queue, next, discovered.
    let phantom = heap.alloc_object(ClassId::new(0), 4);
    let kept_phantom = heap.alloc_object(ClassId::new(0), 4);
    let weak = heap.alloc_object(ClassId::new(0), 4);
    let queue = heap.alloc_object(ClassId::new(0), 3); // head, length, lock
    let dead = heap.alloc_object(ClassId::new(0), 1);
    let dead_weakly = heap.alloc_object(ClassId::new(0), 1);
    let live = heap.alloc_object(ClassId::new(0), 1);
    for (r, referent) in [(phantom, dead), (kept_phantom, live), (weak, dead_weakly)] {
        heap.set_field(r, 0, Value::Object(Some(referent)));
        heap.set_field(r, 1, Value::Object(Some(queue)));
    }
    heap.set_field(queue, 1, Value::Long(0));
    {
        let mut proc = shared.mem.ref_processor.lock();
        proc.discover_reference(
            ReferenceType::Phantom,
            addr(phantom),
            addr(dead),
            Some(addr(queue)),
        );
        proc.discover_reference(
            ReferenceType::Phantom,
            addr(kept_phantom),
            addr(live),
            Some(addr(queue)),
        );
        proc.discover_reference(ReferenceType::Weak, addr(weak), addr(dead_weakly), None);
    }
    let (dead_addr, dead_weakly_addr) = (addr(dead), addr(dead_weakly));
    let is_marked = |a: usize| a != dead_addr && a != dead_weakly_addr;

    let rows = remark_reference_rows(&shared, &is_marked, &[]);

    let mut retired = rows.retired.clone();
    retired.sort_unstable();
    let mut want = vec![addr(phantom), addr(weak)];
    want.sort_unstable();
    assert_eq!(
        retired, want,
        "{gc_algorithm:?}: the round retires both dead-referent rows"
    );
    assert_eq!(
        heap.get_field(phantom, 0),
        Value::Object(None),
        "{gc_algorithm:?}: an enqueued phantom kept a slot 0 naming its dead referent"
    );
    assert_eq!(
        heap.get_field(queue, 0),
        Value::Object(Some(phantom)),
        "{gc_algorithm:?}: the phantom was linked into its queue"
    );
    assert_eq!(heap.get_field(weak, 0), Value::Object(None));
    assert_eq!(
        heap.get_field(kept_phantom, 0),
        Value::Object(Some(live)),
        "{gc_algorithm:?}: a phantom whose referent is marked stays active, slot intact"
    );
}

#[test]
fn g1_remark_nulls_the_slot_of_a_phantom_it_enqueues() {
    remark_nulls_enqueued_phantom_slots_on(GcAlgorithm::G1);
}

#[test]
fn generational_remark_nulls_the_slot_of_a_phantom_it_enqueues() {
    remark_nulls_enqueued_phantom_slots_on(GcAlgorithm::Generational);
}
