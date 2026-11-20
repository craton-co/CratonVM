use super::*;

/// The pre-collection pass binds a constructed allocation-registered
/// `Reference` from its slots, requeues one whose constructor has not run
/// (null queue slot), never discovers a null referent, drops an address
/// that holds another class now, and leaves a bridge-registered one alone.
#[test]
fn gcg_g3rw_allocated_references_are_bound_by_the_pre_collection_pass() {
    use cratonvm_gc::reference::ReferenceType;
    let shared =
        std::sync::Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
    let heap = &shared.mem.heap;
    let addr = |o: ObjectRef| o.as_ptr() as usize;
    let referent = heap.alloc_object(ClassId::new(0), 1);
    let queue = heap.alloc_object(ClassId::new(0), 1);
    let constructed = heap.alloc_object(ClassId::new(0), 2);
    let unconstructed = heap.alloc_object(ClassId::new(0), 2);
    let null_referent = heap.alloc_object(ClassId::new(0), 2);
    let other_class = heap.alloc_object(ClassId::new(0), 2);
    let bridged = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field(constructed, 0, Value::Object(Some(referent)));
    heap.set_field(constructed, 1, Value::Object(Some(queue)));
    heap.set_field(null_referent, 1, Value::Object(Some(queue)));
    heap.set_field(other_class, 0, Value::Object(Some(referent)));
    heap.set_field(other_class, 1, Value::Object(Some(queue)));
    heap.set_field(bridged, 0, Value::Object(Some(referent)));
    heap.set_field(bridged, 1, Value::Object(Some(queue)));
    let cid = heap.class_id_of(constructed).as_u32();
    {
        let mut rp = shared.mem.ref_processor.lock();
        for o in [constructed, unconstructed, null_referent, bridged] {
            rp.note_allocated_reference(ReferenceType::Weak, addr(o), cid, false);
        }
        rp.note_allocated_reference(ReferenceType::Weak, addr(other_class), cid + 1, false);
        rp.discover_reference(ReferenceType::Weak, addr(bridged), addr(referent), None);
    }

    bind_allocated_references_pre_gc(&shared);

    let mut rp = shared.mem.ref_processor.lock();
    let pairs = rp.weak_phantom_active_pairs();
    assert!(
        pairs.contains(&(addr(constructed), addr(referent))),
        "bound: {pairs:x?}"
    );
    assert!(pairs.contains(&(addr(bridged), addr(referent))));
    assert_eq!(
        rp.weak_ref_count(),
        2,
        "the bridged one is not registered twice"
    );
    assert!(
        !rp.has_active_reference_row(addr(null_referent)),
        "never discovered"
    );
    assert!(
        !rp.has_active_reference_row(addr(other_class)),
        "screened by class"
    );
    assert_eq!(rp.allocated_unbound_len(), 1, "the unconstructed one waits");
    assert_eq!(
        rp.take_allocated_unbound()[0].row.reference_obj,
        addr(unconstructed)
    );
    assert_eq!(
        rp.identity_stamp(addr(constructed)).is_some(),
        !heap.zgc_persistent_skip_bits_enabled(),
        "stamping follows the collector policy"
    );
}
