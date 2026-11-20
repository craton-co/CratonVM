use super::*;

fn oref(addr: usize) -> cratonvm_types::ObjectRef {
    // Never dereferenced: the store only compares and forwards addresses.
    unsafe { cratonvm_types::ObjectRef::from_raw(addr as *mut u8) }
}

fn scan(vm: usize) -> Vec<(Option<u32>, usize)> {
    let mut out = Vec::new();
    gc_scan_class_atomic_side_store(vm, &mut |owner, v| out.push((owner, v.as_ptr() as usize)));
    out.sort();
    out
}

/// Two classes whose mirrors share an identity hash share a key. The old
/// store served class B class A's `ReflectionData`, and B's `null -> x`
/// CAS failed forever against A's value.
#[test]
fn an_identity_hash_collision_is_a_miss_not_another_classes_value() {
    const VM: usize = 0xC1A0_0001;
    let key: ClassAtomicKey = (42, 0);
    class_atomic_put(VM, key, Some(7), Some(oref(0x1000)));
    assert_eq!(
        class_atomic_get(VM, key, Some(8)),
        None,
        "class 8 must not see class 7's slot"
    );
    assert!(
        class_atomic_cas(VM, key, Some(8), None, Some(oref(0x2000))),
        "the colliding class's null -> value CAS must succeed"
    );
    assert_eq!(
        class_atomic_get(VM, key, Some(8)).map(|o| o.as_ptr() as usize),
        Some(0x2000)
    );
    assert_eq!(class_atomic_get(VM, key, Some(7)), None);
    forget_vm_class_atomic_side_store(VM);
}

/// Ordinary CAS semantics within one class.
#[test]
fn cas_succeeds_only_against_the_current_value() {
    const VM: usize = 0xC1A0_0002;
    let key: ClassAtomicKey = (5, 2);
    assert!(class_atomic_cas(VM, key, Some(3), None, Some(oref(0x1000))));
    assert!(!class_atomic_cas(
        VM,
        key,
        Some(3),
        None,
        Some(oref(0x2000))
    ));
    assert!(class_atomic_cas(
        VM,
        key,
        Some(3),
        Some(oref(0x1000)),
        Some(oref(0x3000))
    ));
    assert_eq!(
        class_atomic_get(VM, key, Some(3)).map(|o| o.as_ptr() as usize),
        Some(0x3000)
    );
    forget_vm_class_atomic_side_store(VM);
}

/// A redefinition moves `classRedefinedCount`, so `Class.annotationData()`
/// CASes over its stale, NON-null heap value. With the row pruned (or
/// overwritten by a colliding class) the store holds nothing to compare
/// against; failing would livelock the JDK's retry loop, which re-reads
/// the same heap value forever (interpreter round i1 wave 27, lane L3).
#[test]
fn a_cas_over_a_value_the_store_no_longer_holds_succeeds() {
    const VM: usize = 0xC1A0_0007;
    let key: ClassAtomicKey = (11, 2);
    assert!(
        class_atomic_cas(VM, key, Some(4), Some(oref(0x1000)), Some(oref(0x2000))),
        "no row of class 4: the CAS succeeds"
    );
    assert_eq!(
        class_atomic_get(VM, key, Some(4)).map(|o| o.as_ptr() as usize),
        Some(0x2000)
    );
    assert!(
        !class_atomic_cas(VM, key, Some(4), Some(oref(0x1000)), Some(oref(0x3000))),
        "with a row, a stale expected value still fails"
    );
    forget_vm_class_atomic_side_store(VM);
}

/// Another VM's rows are neither visible, scanned nor pruned.
#[test]
fn rows_are_per_vm() {
    const A: usize = 0xC1A0_0003;
    const B: usize = 0xC1A0_0004;
    let key: ClassAtomicKey = (9, 0);
    class_atomic_put(A, key, Some(1), Some(oref(0x1000)));
    assert_eq!(class_atomic_get(B, key, Some(1)), None);
    assert!(scan(B).is_empty());
    assert_eq!(prune_class_atomic_side_store(B, &|_| true, &|_| false), 0);
    assert_eq!(scan(A), vec![(Some(1), 0x1000)]);
    forget_vm_class_atomic_side_store(A);
    assert!(scan(A).is_empty());
}

/// The scan hands the VM each value WITH its owner (what lets the root
/// source pin it to the owner's loader); prune drops rows of dead classes
/// and rows whose value died, and keeps the rest.
#[test]
fn scan_reports_owners_and_prune_drops_dead_classes_and_dead_values() {
    const VM: usize = 0xC1A0_0005;
    class_atomic_put(VM, (1, 0), Some(10), Some(oref(0x1000)));
    class_atomic_put(VM, (2, 0), Some(11), Some(oref(0x2000)));
    class_atomic_put(VM, (3, 2), None, Some(oref(0x3000)));
    class_atomic_put(VM, (4, 1), Some(12), None);
    assert_eq!(
        scan(VM),
        vec![(None, 0x3000), (Some(10), 0x1000), (Some(11), 0x2000)],
        "a null slot is not a root"
    );
    // Class 10 unloaded; the value at 0x3000 did not survive.
    let dropped = prune_class_atomic_side_store(VM, &|id| id == 10, &|addr| addr != 0x3000);
    assert_eq!(dropped, 2);
    assert_eq!(scan(VM), vec![(Some(11), 0x2000)]);
    assert_eq!(
        class_atomic_get(VM, (4, 1), Some(12)),
        None,
        "the null slot of a live class is kept (and still reads null)"
    );
    forget_vm_class_atomic_side_store(VM);
}

/// The remap forwards moved values and leaves the rest alone.
#[test]
fn remap_forwards_moved_values_only() {
    const VM: usize = 0xC1A0_0006;
    class_atomic_put(VM, (1, 0), Some(10), Some(oref(0x1000)));
    class_atomic_put(VM, (2, 0), Some(11), Some(oref(0x2000)));
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x1000, 0x9000);
    gc_update_class_atomic_side_store_refs(VM, &map);
    assert_eq!(scan(VM), vec![(Some(10), 0x9000), (Some(11), 0x2000)]);
    forget_vm_class_atomic_side_store(VM);
}
