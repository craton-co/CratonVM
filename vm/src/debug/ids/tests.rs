use super::*;

/// Interpreter round i5 wave 104 (lane L1): the `DestroyJavaVM` split
/// maps VM thread 0 to its own wire id and `main`'s to the ended `main`.
#[test]
fn the_destroy_java_vm_split_gives_each_thread_its_own_id() {
    assert_eq!(thread_to_wire_split(false, 0), MAIN_THREAD_WIRE_ID);
    assert_eq!(thread_from_wire_split(false, MAIN_THREAD_WIRE_ID), 0);
    assert_eq!(
        thread_from_wire_split(false, DESTROY_JAVA_VM_WIRE_ID),
        DESTROY_JAVA_VM_WIRE_ID
    );
    assert_eq!(thread_to_wire_split(true, 0), DESTROY_JAVA_VM_WIRE_ID);
    assert_eq!(thread_from_wire_split(true, DESTROY_JAVA_VM_WIRE_ID), 0);
    assert_eq!(
        thread_from_wire_split(true, MAIN_THREAD_WIRE_ID),
        ENDED_MAIN_VM_ID
    );
    assert_eq!(
        thread_to_wire_split(true, ENDED_MAIN_VM_ID),
        MAIN_THREAD_WIRE_ID
    );
    assert_eq!((thread_to_wire_split(true, 7), thread_from_wire_split(true, 7)), (7, 7));
    assert_eq!(thread_from_wire_split(true, 0), NO_VM_ID);
}

#[test]
fn register_and_lookup_object() {
    let mut mgr = IdManager::new();
    let id = mgr.register_object(0xDEAD);
    assert_eq!(id, ObjectId(1));
    assert_eq!(mgr.lookup_object(id), Some(0xDEAD));
}

#[test]
fn duplicate_object_returns_same_id() {
    let mut mgr = IdManager::new();
    let a = mgr.register_object(42);
    let b = mgr.register_object(42);
    assert_eq!(a, b);
}

#[test]
fn lookup_missing_object_returns_none() {
    let mgr = IdManager::new();
    assert_eq!(mgr.lookup_object(ObjectId(999)), None);
}

#[test]
fn register_and_lookup_thread() {
    let mut mgr = IdManager::new();
    let t = mgr.register_thread(7);
    assert_eq!(mgr.lookup_thread(t), Some(7));
}

#[test]
fn all_thread_ids_returns_registered() {
    let mut mgr = IdManager::new();
    mgr.register_thread(1);
    mgr.register_thread(2);
    let ids = mgr.all_thread_ids();
    assert_eq!(ids.len(), 2);
}

#[test]
fn register_ref_type_and_method() {
    let mut mgr = IdManager::new();
    let rt = mgr.register_ref_type(100);
    let m = mgr.register_method(200);
    assert_eq!(mgr.lookup_ref_type(rt), Some(100));
    assert_eq!(mgr.lookup_method(m), Some(200));
}

#[test]
fn register_and_lookup_field() {
    let mut mgr = IdManager::new();
    let f = mgr.register_field(300);
    assert_eq!(mgr.lookup_field(f), Some(300));
}

/// Wave 7: an object keeps its id across a collection that moves it (the
/// address index is rebuilt from the handles), a disposed id is gone once
/// every send is disposed, and a snapshot pin alone never outlives the
/// snapshot.
#[test]
fn object_ids_survive_moves_and_honour_dispose_counts() {
    let mut t = ObjectTable::new();
    // handle 0xA0 refers to an object at 0x1000.
    assert!(t.needs_rekey(3));
    t.rekey(3, |_| None);
    assert_eq!(t.lookup_addr(0x1000, ObjectExport::Sent), None);
    t.insert(7, 0xA0, 0x1000, ObjectExport::Sent);
    assert_eq!(
        t.lookup_addr(0x1000, ObjectExport::Sent),
        Some(7),
        "same object, same id"
    );
    // A collection moved it to 0x2000, and put another object at 0x1000.
    assert!(t.needs_rekey(4));
    t.rekey(4, |h| (h == 0xA0).then_some(0x2000));
    assert_eq!(
        t.lookup_addr(0x1000, ObjectExport::Sent),
        None,
        "not the old address"
    );
    assert_eq!(
        t.lookup_addr(0x2000, ObjectExport::Sent),
        Some(7),
        "the moved object"
    );
    assert_eq!(t.handle_of(7), Some(0xA0));
    // Sent three times: disposing two keeps it, the third releases it.
    t.dispose(7, 2);
    assert!(t.contains(7));
    t.dispose(7, 1);
    assert!(!t.contains(7));
    assert_eq!(t.take_released(), vec![0xA0]);
    assert!(t.take_released().is_empty());
    // A pinned id that no command handed out goes with the snapshot; one
    // that was handed out stays.
    t.insert(8, 0xB0, 0x3000, ObjectExport::Pinned);
    t.insert(9, 0xC0, 0x4000, ObjectExport::Pinned);
    t.note_sent(9);
    t.unpin(8);
    t.unpin(9);
    assert!(!t.contains(8) && t.contains(9));
    assert_eq!(t.take_released(), vec![0xB0]);
    t.release_all();
    assert!(t.is_empty());
    assert_eq!(t.take_released(), vec![0xC0]);
}

/// Wave 8: a collected referent keeps its entry (marked, unindexed) until
/// disposed; a stale index entry can be checked before it is counted;
/// collection enable/disable counts nest; a replaced handle is released.
#[test]
fn weak_object_ids_mark_collection_and_count_collection_disables() {
    let mut t = ObjectTable::new();
    t.rekey(1, |_| None);
    t.insert(7, 0xA0, 0x1000, ObjectExport::Sent);
    t.insert(8, 0xB0, 0x2000, ObjectExport::Sent);
    // The collection took 7's referent; 8 moved.
    t.rekey(2, |h| (h == 0xB0).then_some(0x3000));
    assert!(t.contains(7), "a collected id stays until disposed");
    assert_eq!(t.candidate_for_addr(0x1000), None, "and is not indexed");
    assert_eq!(t.candidate_for_addr(0x3000), Some((8, 0xB0)));
    // Counting is separate from the lookup.
    t.note_export(8, ObjectExport::Sent);
    t.dispose(8, 1);
    assert!(t.contains(8), "sent twice, disposed once");
    t.forget_addr(8);
    assert_eq!(t.candidate_for_addr(0x3000), None);
    assert_eq!(t.disable_collection(8), Some(1));
    assert_eq!(t.disable_collection(8), Some(2));
    assert_eq!(t.enable_collection(8), Some(1));
    assert_eq!(t.enable_collection(8), Some(0));
    assert_eq!(t.enable_collection(8), Some(0), "never below zero");
    assert_eq!(t.disable_collection(99), None);
    t.replace_handle(8, 0xC0);
    assert_eq!(t.handle_of(8), Some(0xC0));
    assert_eq!(t.take_released(), vec![0xB0]);
    t.dispose(7, 1);
    t.dispose(8, 1);
    assert!(t.is_empty());
    assert_eq!(t.take_released(), vec![0xA0, 0xC0]);
}

#[test]
fn fresh_object_ids_share_the_register_object_counter() {
    let mut mgr = IdManager::new();
    let a = mgr.register_object(0x55);
    let b = mgr.fresh_object_id();
    let c = mgr.register_object(0x66);
    assert!(a != b && b != c && a != c);
    assert_eq!(mgr.lookup_object(b), None, "a fresh id has no mapping here");
}

#[test]
fn ids_start_at_one() {
    let mut mgr = IdManager::new();
    assert_eq!(mgr.register_object(0), ObjectId(1));
    assert_eq!(mgr.register_thread(0), ThreadId(1));
    assert_eq!(mgr.register_ref_type(0), ReferenceTypeId(1));
    assert_eq!(mgr.register_method(0), MethodId(1));
    assert_eq!(mgr.register_field(0), FieldId(1));
}
